#!/usr/bin/env python3
"""Locale-catalogue gate for the RecurAgent TUI.

Three checks, each one pinning a failure mode that is invisible until a user
sees it:

1. KEY PARITY — every key must carry every locale listed under
   `[package.metadata.i18n] available-locales`. Keys are symbolic
   (`login_flow_logout_absent`), not the English source text, so a lookup that
   misses renders the KEY ITSELF. A missing `zh-CN` entry is therefore a
   user-visible string `login_flow_logout_absent`, not a quiet English
   fallback. There is no soft landing, which is why this is a gate and not a
   warning.

2. PLACEHOLDER PARITY — the set of `%{name}` placeholders must match across
   every locale of a key. A translator who drops `%{provider}` produces a
   message that silently fails to say WHICH provider failed; nothing errors,
   the sentence is just wrong.

3. NO NEW BARE CJK LITERALS — user-facing Chinese must live in the catalogue,
   not inline in `src/`. Two mechanisms in one tree is how "the same tool
   implemented twice" starts. An exact baseline is tolerated (the ~40 literals
   that predate the catalogue) and can only go down.

Usage:
    python3 scripts/check_i18n_keys.py
    python3 scripts/check_i18n_keys.py --update-baseline   # after shrinking
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCALES = ROOT / "locales"

# Tolerated pre-existing inline CJK literals in the TUI surface. This number
# may only ever be lowered: it records debt the catalogue has not yet absorbed.
#
# Currently 4, all in `dag_view.rs`: the `START_LABEL`/`END_LABEL` constants
# ("开始"/"结束") and the two `… 其余 {omitted} 个节点已省略` omission strings.
# Those are genuinely user-facing and should move into the catalogue — the
# baseline exists so they can be found, not so they can stay.
#
# The count is measured with `#[cfg(test)]` regions excluded: a Chinese literal
# in a test is fixture data (CJK width cases, IME paste cases), not chrome.
CJK_BASELINE = 4

# Files whose inline CJK is counted by check 3. The catalogue itself is
# obviously excluded, as are tests, where a Chinese fixture is data rather than
# chrome a user reads.
UI_GLOBS = [
    "src/interactive_ftui.rs",
    "src/interactive_ftui/*.rs",
    "src/interactive.rs",
    "src/interactive/*.rs",
    "src/tui.rs",
    "src/dag_view.rs",
    "src/status_line.rs",
    "src/delight.rs",
    "src/theme.rs",
    "src/autocomplete.rs",
    "src/completions.rs",
]

HAN = re.compile(r"[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]")
# A string literal, either bare or raw. Deliberately simple: this looks for
# Han characters between quotes and nothing else.
STRING_LIT = re.compile(r'"((?:[^"\\]|\\.)*)"')
# `%{name}` is rust-i18n's placeholder form.
PLACEHOLDER = re.compile(r"%\{([A-Za-z_][A-Za-z0-9_]*)\}")


def fail(lines: list[str]) -> None:
    for line in lines:
        print(line, file=sys.stderr)


def read_available_locales() -> list[str]:
    """`available-locales` from the `[package.metadata.i18n]` table."""
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(
        r"\[package\.metadata\.i18n\](.*?)(?=\n\[|\Z)", cargo, re.DOTALL
    )
    if not match:
        fail(["check_i18n: [package.metadata.i18n] is missing from Cargo.toml"])
        sys.exit(2)
    listed = re.search(r"available-locales\s*=\s*\[(.*?)\]", match.group(1), re.DOTALL)
    if not listed:
        fail(["check_i18n: available-locales is missing from [package.metadata.i18n]"])
        sys.exit(2)
    return re.findall(r'"([^"]+)"', listed.group(1))


def parse_catalogue(path: Path) -> tuple[dict[str, dict[str, str]], list[str]]:
    """Parse the restricted `_version: 2` shape this repo uses.

    Only the two constructs we actually write are understood: a top-level
    `key:` line and an indented `<locale>: <value>` line. Anything else is an
    error rather than a silent skip, because a silently-skipped entry is a key
    that passes parity while shipping nothing.
    """
    entries: dict[str, dict[str, str]] = {}
    problems: list[str] = []
    current: str | None = None

    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.rstrip()
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith("_version:"):
            continue
        indent = len(line) - len(line.lstrip())
        stripped = line.strip()

        if indent == 0:
            if not stripped.endswith(":"):
                problems.append(f"{path.name}:{number}: unrecognised top-level line {stripped!r}")
                current = None
                continue
            current = stripped[:-1].strip()
            if current in entries:
                problems.append(f"{path.name}:{number}: duplicate key {current!r}")
            entries.setdefault(current, {})
            continue

        if current is None:
            problems.append(f"{path.name}:{number}: {stripped!r} before any key")
            continue

        locale_match = re.match(r"([A-Za-z][A-Za-z0-9-]*)\s*:\s*(.*)$", stripped)
        if not locale_match:
            problems.append(f"{path.name}:{number}: unrecognised entry {stripped!r}")
            continue
        locale, value = locale_match.group(1), locale_match.group(2).strip()

        if len(value) >= 2 and value[0] == '"' and value[-1] == '"':
            # Double-quoted scalar: the only escape we emit is \n.
            value = value[1:-1].replace("\\n", "\n").replace('\\"', '"')
        elif value.startswith('"') or value.endswith('"'):
            problems.append(f"{path.name}:{number}: unbalanced quotes in {stripped!r}")
            continue

        entries[current][locale] = value

    return entries, problems


def check_parity(entries: dict[str, dict[str, str]], locales: list[str]) -> list[str]:
    problems = []
    for key, values in sorted(entries.items()):
        missing = [locale for locale in locales if locale not in values]
        if missing:
            problems.append(
                f"key {key!r} is missing {missing} — a lookup would render the key "
                f"itself on screen (has: {sorted(values)})"
            )
        extra = [locale for locale in values if locale not in locales]
        if extra:
            problems.append(
                f"key {key!r} carries {extra}, which are not in "
                f"[package.metadata.i18n] available-locales"
            )
    return problems


def check_placeholders(entries: dict[str, dict[str, str]]) -> list[str]:
    problems = []
    for key, values in sorted(entries.items()):
        per_locale = {
            locale: set(PLACEHOLDER.findall(text)) for locale, text in values.items()
        }
        if len(per_locale) < 2:
            continue
        reference_locale, reference = next(iter(sorted(per_locale.items())))
        for locale, names in sorted(per_locale.items()):
            if names != reference:
                dropped = sorted(reference - names)
                added = sorted(names - reference)
                problems.append(
                    f"key {key!r}: {locale} placeholders differ from {reference_locale} "
                    f"(missing {dropped}, unexpected {added}) — the call site passes "
                    f"{sorted(reference)}, so the message would silently omit them"
                )
    return problems


def check_inline_cjk() -> tuple[int, list[str]]:
    offenders: list[str] = []
    total = 0
    for pattern in UI_GLOBS:
        for path in sorted(ROOT.glob(pattern)):
            if not path.is_file() or "/tests/" in path.as_posix():
                continue
            if path.name == "tests.rs":
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            # A Chinese literal inside `#[cfg(test)]` is test DATA — CJK width
            # cases, IME paste cases, `模型ab` fixtures — not chrome a user
            # reads. Cut at the first test attribute instead of brace-tracking:
            # `mod tests` is the last item in every file this gate scans.
            test_at = text.find("#[cfg(test)]")
            if test_at != -1:
                text = text[:test_at]
            for number, line in enumerate(text.splitlines(), 1):
                code = line.split("//", 1)[0]
                for literal in STRING_LIT.findall(code):
                    if HAN.search(literal):
                        total += 1
                        offenders.append(
                            f"{path.relative_to(ROOT)}:{number}: {literal[:70]!r}"
                        )
    return total, offenders


def main() -> int:
    update = "--update-baseline" in sys.argv
    locales = read_available_locales()
    files = sorted(LOCALES.glob("*.yml"))
    if not files:
        fail([f"check_i18n: no catalogue under {LOCALES}"])
        return 2

    problems: list[str] = []
    entries: dict[str, dict[str, str]] = {}
    for path in files:
        parsed, parse_problems = parse_catalogue(path)
        problems.extend(parse_problems)
        for key, values in parsed.items():
            if key in entries:
                problems.append(f"key {key!r} is defined in more than one catalogue file")
            entries[key] = values

    problems.extend(check_parity(entries, locales))
    problems.extend(check_placeholders(entries))

    total_cjk, offenders = check_inline_cjk()
    cjk_ok = total_cjk <= CJK_BASELINE
    if not cjk_ok:
        problems.append(
            f"{total_cjk} inline CJK literals in the TUI surface, baseline is "
            f"{CJK_BASELINE} — new user-facing Chinese belongs in {LOCALES.name}/, "
            f"not inline. Offenders:\n    " + "\n    ".join(offenders)
        )

    if problems:
        fail(problems)
        fail([f"\ncheck_i18n: FAILED ({len(problems)} problem(s))"])
        return 1

    print(
        f"check_i18n: ok — {len(entries)} keys x {len(locales)} locales "
        f"({', '.join(locales)}), inline CJK {total_cjk}/{CJK_BASELINE}"
    )
    if update:
        print(
            f"check_i18n: baseline unchanged at {CJK_BASELINE} "
            f"(would be {total_cjk}); edit CJK_BASELINE in this script to lower it"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
