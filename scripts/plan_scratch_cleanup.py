#!/usr/bin/env python3
"""Plan scratch cleanup without deleting files.

This script inventories matching entries under approved scratch roots and emits
operator evidence that is safe to review before asking for explicit deletion
approval. It never removes files and intentionally has no apply mode.

Usage:
  python3 scripts/plan_scratch_cleanup.py
  python3 scripts/plan_scratch_cleanup.py --root /tmp --pattern 'franken*' --json
  python3 scripts/plan_scratch_cleanup.py --self-test
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import stat as stat_module
import sys
from collections import Counter
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, Iterable

SCHEMA = "ra.scratch_cleanup_plan.v1"
OWNER_MARKER_SCHEMA = "ra.scratch_target_owner.v1"
DEFAULT_CARGO_ROOT = "/data/tmp/recur_agent_cargo"
DEFAULT_ROOTS = ("/tmp", DEFAULT_CARGO_ROOT)
DEFAULT_PATTERNS = ("franken*", "recur_agent*", "pi-agent-rust*")
OWNER_MARKER_FILES = (".pi-agent-target.json", ".recur_agent_target_owner.json")
OWNER_MARKER_MAX_BYTES = 64 * 1024


@dataclass(frozen=True)
class OwnerMarker:
    status: str
    agent_name: str
    marker_path: str
    schema: str
    expires_at: str
    project_key: str
    purpose: str
    reason: str


@dataclass(frozen=True)
class EntryPlan:
    path: str
    root: str
    name: str
    kind: str
    matched_pattern: str
    shallow_bytes: int
    mtime_epoch: float
    age_seconds: int
    owner_hint: str
    group: str
    owner_marker_status: str
    owner_marker_agent: str
    owner_marker_path: str
    owner_marker_schema: str
    owner_marker_expires_at: str
    cleanup_safety: str
    review_action: str


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def parse_iso_epoch(value: str) -> float | None:
    try:
        normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
        return datetime.fromisoformat(normalized).timestamp()
    except ValueError:
        return None


def is_allowed_root(path: Path, allowed_roots: Iterable[Path]) -> bool:
    try:
        resolved = path.resolve(strict=False)
    except OSError:
        return False
    for allowed in allowed_roots:
        try:
            allowed_resolved = allowed.resolve(strict=False)
        except OSError:
            continue
        if resolved == allowed_resolved or allowed_resolved in resolved.parents:
            return True
    return False


def is_default_cargo_root(root: Path) -> bool:
    try:
        return root.resolve(strict=False) == Path(DEFAULT_CARGO_ROOT).resolve(strict=False)
    except OSError:
        return False


def patterns_for_root(root: Path, patterns: tuple[str, ...]) -> tuple[str, ...]:
    if patterns == DEFAULT_PATTERNS and is_default_cargo_root(root):
        return ("*",)
    return patterns


def entry_kind(path: Path) -> str:
    try:
        if path.is_symlink():
            return "symlink"
        if path.is_dir():
            return "directory"
        if path.is_file():
            return "file"
    except OSError:
        return "unreadable"
    return "other"


def classify_group(name: str) -> str:
    lowered = name.lower()
    if lowered.startswith("franken_engine") or lowered.startswith("franken-engine"):
        return "franken_engine"
    if lowered.startswith("franken_node") or lowered.startswith("franken-node"):
        return "franken_node"
    if lowered.startswith("recur_agent") or lowered.startswith("pi-agent-rust"):
        return "recur_agent"
    if lowered.startswith("franken"):
        return "franken_other"
    return "other"


def owner_hint(root: Path, path: Path) -> str:
    try:
        relative_parts = path.relative_to(root).parts
    except ValueError:
        relative_parts = path.parts
    if str(root).rstrip("/") == "/data/tmp/recur_agent_cargo" and relative_parts:
        return relative_parts[0] or "unknown"
    name = path.name
    for marker in ("codex", "claude", "agent", "ubuntu"):
        if marker in name.lower():
            return marker
    return "unknown"


def matched_pattern(name: str, patterns: Iterable[str]) -> str | None:
    for pattern in patterns:
        if fnmatch.fnmatchcase(name, pattern):
            return pattern
    return None


def empty_owner_marker(status: str, reason: str) -> OwnerMarker:
    return OwnerMarker(
        status=status,
        agent_name="unknown",
        marker_path="",
        schema="",
        expires_at="",
        project_key="",
        purpose="",
        reason=reason,
    )


def parse_owner_marker(marker_path: Path, now_epoch: float) -> OwnerMarker:
    try:
        marker_stat = marker_path.lstat()
    except OSError as exc:
        return empty_owner_marker("malformed", f"marker stat failed: {exc}")
    if not stat_module.S_ISREG(marker_stat.st_mode):
        return empty_owner_marker("malformed", "marker is not a regular file")
    if marker_stat.st_size > OWNER_MARKER_MAX_BYTES:
        return empty_owner_marker("malformed", "marker exceeds max size")

    try:
        with marker_path.open("rb") as handle:
            raw = handle.read(OWNER_MARKER_MAX_BYTES + 1)
    except OSError as exc:
        return empty_owner_marker("malformed", f"marker read failed: {exc}")
    if len(raw) > OWNER_MARKER_MAX_BYTES:
        return empty_owner_marker("malformed", "marker exceeds max size")

    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        return empty_owner_marker("malformed", f"marker json failed: {exc}")
    if not isinstance(payload, dict):
        return empty_owner_marker("malformed", "marker payload is not an object")

    schema = payload.get("schema")
    if schema != OWNER_MARKER_SCHEMA:
        return empty_owner_marker("malformed", "marker schema mismatch")

    active = payload.get("active", True)
    if not isinstance(active, bool):
        return empty_owner_marker("malformed", "marker active field is not boolean")

    expires_at = payload.get("expires_at", "")
    expires_epoch = None
    if expires_at:
        if not isinstance(expires_at, str):
            return empty_owner_marker("malformed", "marker expires_at field is not string")
        expires_epoch = parse_iso_epoch(expires_at)
        if expires_epoch is None:
            return empty_owner_marker("malformed", "marker expires_at field is invalid")

    agent_name = payload.get("agent_name", "unknown")
    if not isinstance(agent_name, str) or not agent_name.strip():
        agent_name = "unknown"
    project_key = payload.get("project_key", "")
    if not isinstance(project_key, str):
        project_key = ""
    purpose = payload.get("purpose", "")
    if not isinstance(purpose, str):
        purpose = ""

    if not active:
        status = "inactive"
        reason = "marker active field is false"
    elif expires_epoch is not None and expires_epoch <= now_epoch:
        status = "expired"
        reason = "marker expires_at is in the past"
    else:
        status = "active"
        reason = "marker is active"

    return OwnerMarker(
        status=status,
        agent_name=agent_name,
        marker_path=str(marker_path),
        schema=OWNER_MARKER_SCHEMA,
        expires_at=expires_at,
        project_key=project_key,
        purpose=purpose,
        reason=reason,
    )


def read_owner_marker(path: Path, kind: str, now_epoch: float) -> OwnerMarker:
    if kind != "directory":
        return empty_owner_marker("not_applicable", "entry is not a directory")
    for marker_name in OWNER_MARKER_FILES:
        marker_path = path / marker_name
        try:
            if marker_path.exists() or marker_path.is_symlink():
                marker = parse_owner_marker(marker_path, now_epoch)
                if marker.marker_path:
                    return marker
                return OwnerMarker(
                    status=marker.status,
                    agent_name=marker.agent_name,
                    marker_path=str(marker_path),
                    schema=marker.schema,
                    expires_at=marker.expires_at,
                    project_key=marker.project_key,
                    purpose=marker.purpose,
                    reason=marker.reason,
                )
        except OSError as exc:
            return empty_owner_marker("malformed", f"marker lookup failed: {exc}")
    return empty_owner_marker("missing", "no owner marker present")


def cleanup_safety_for_marker(marker: OwnerMarker) -> str:
    if marker.status == "active":
        return "blocked_active_owner_marker"
    if marker.status in {"missing", "not_applicable"}:
        return "unknown_owner_fail_closed"
    if marker.status == "malformed":
        return "unknown_owner_malformed_marker_fail_closed"
    if marker.status in {"expired", "inactive"}:
        return "expired_or_inactive_marker_manual_review"
    return "unknown_owner_fail_closed"


def review_action_for_marker(marker: OwnerMarker) -> str:
    if marker.status == "active":
        return "do_not_delete_active_owner_marker"
    if marker.status == "malformed":
        return "manual_review_malformed_owner_marker"
    if marker.status in {"expired", "inactive"}:
        return "manual_review_expired_owner_marker"
    return "manual_review_unknown_owner"


def scan_root(
    root: Path,
    patterns: tuple[str, ...],
    now_epoch: float,
    min_age_seconds: int,
    allowed_roots: tuple[Path, ...],
) -> tuple[list[EntryPlan], list[str]]:
    warnings: list[str] = []
    entries: list[EntryPlan] = []

    if not is_allowed_root(root, allowed_roots):
        warnings.append(f"refusing root outside allowlist: {root}")
        return entries, warnings
    if not root.exists():
        warnings.append(f"root does not exist: {root}")
        return entries, warnings
    if not root.is_dir():
        warnings.append(f"root is not a directory: {root}")
        return entries, warnings
    effective_patterns = patterns_for_root(root, patterns)

    try:
        with os.scandir(root) as iterator:
            dir_entries = list(iterator)
    except OSError as exc:
        warnings.append(f"unable to scan {root}: {exc}")
        return entries, warnings

    for dir_entry in sorted(dir_entries, key=lambda item: item.name):
        pattern = matched_pattern(dir_entry.name, effective_patterns)
        if pattern is None:
            continue
        path = Path(dir_entry.path)
        try:
            stat = dir_entry.stat(follow_symlinks=False)
        except OSError as exc:
            warnings.append(f"unable to stat {path}: {exc}")
            continue
        age_seconds = max(0, int(now_epoch - stat.st_mtime))
        if age_seconds < min_age_seconds:
            continue
        kind = entry_kind(path)
        marker = read_owner_marker(path, kind, now_epoch)
        entries.append(
            EntryPlan(
                path=str(path),
                root=str(root),
                name=dir_entry.name,
                kind=kind,
                matched_pattern=pattern,
                shallow_bytes=max(0, int(stat.st_size)),
                mtime_epoch=stat.st_mtime,
                age_seconds=age_seconds,
                owner_hint=owner_hint(root, path),
                group=classify_group(dir_entry.name),
                owner_marker_status=marker.status,
                owner_marker_agent=marker.agent_name,
                owner_marker_path=marker.marker_path,
                owner_marker_schema=marker.schema,
                owner_marker_expires_at=marker.expires_at,
                cleanup_safety=cleanup_safety_for_marker(marker),
                review_action=review_action_for_marker(marker),
            )
        )
    return entries, warnings


def build_plan(
    roots: list[Path],
    patterns: tuple[str, ...],
    min_age_seconds: int,
    allowed_roots: tuple[Path, ...],
    entry_limit: int,
) -> dict[str, Any]:
    now_epoch = datetime.now(timezone.utc).timestamp()
    entries: list[EntryPlan] = []
    warnings: list[str] = []
    for root in roots:
        root_entries, root_warnings = scan_root(
            root,
            patterns,
            now_epoch,
            min_age_seconds,
            allowed_roots,
        )
        entries.extend(root_entries)
        warnings.extend(root_warnings)

    entries.sort(key=lambda entry: (entry.root, entry.group, entry.name))
    group_counts = Counter(entry.group for entry in entries)
    owner_counts = Counter(entry.owner_hint for entry in entries)
    kind_counts = Counter(entry.kind for entry in entries)
    marker_counts = Counter(entry.owner_marker_status for entry in entries)
    safety_counts = Counter(entry.cleanup_safety for entry in entries)

    limited_entries = entries[:entry_limit] if entry_limit >= 0 else entries
    omitted = max(0, len(entries) - len(limited_entries))
    return {
        "schema": SCHEMA,
        "owner_marker_contract": {
            "schema": OWNER_MARKER_SCHEMA,
            "marker_files": list(OWNER_MARKER_FILES),
            "required_fields": ["schema"],
            "optional_fields": [
                "agent_name",
                "project_key",
                "purpose",
                "created_at",
                "expires_at",
                "active",
            ],
            "active_rule": "active is not false and expires_at is absent or in the future",
            "default_cargo_root_pattern": (
                f"{DEFAULT_CARGO_ROOT} uses '*' when the CLI patterns are left at defaults"
            ),
        },
        "generated_at": utc_now_iso(),
        "destructive_actions_executed": False,
        "delete_apply_mode_available": False,
        "approval_required_for_cleanup": True,
        "arg_max_safe_scan": True,
        "roots": [str(root) for root in roots],
        "patterns": list(patterns),
        "min_age_seconds": min_age_seconds,
        "totals": {
            "matched_entries": len(entries),
            "listed_entries": len(limited_entries),
            "omitted_entries": omitted,
            "shallow_bytes": sum(entry.shallow_bytes for entry in entries),
            "by_group": dict(sorted(group_counts.items())),
            "by_owner_hint": dict(sorted(owner_counts.items())),
            "by_kind": dict(sorted(kind_counts.items())),
            "by_owner_marker_status": dict(sorted(marker_counts.items())),
            "by_cleanup_safety": dict(sorted(safety_counts.items())),
        },
        "entries": [asdict(entry) for entry in limited_entries],
        "warnings": warnings,
        "operator_note": (
            "This is a read-only inventory. Do not remove any listed path without "
            "a separate explicit approval that names the exact cleanup command and risk. "
            "Missing, malformed, expired, or non-directory owner markers are not safe-to-delete proof."
        ),
    }


def render_text(plan: dict[str, Any]) -> str:
    totals = plan["totals"]
    lines = [
        "Scratch Cleanup Plan",
        f"schema: {plan['schema']}",
        "mode: read-only; no destructive actions executed",
        f"matched entries: {totals['matched_entries']}",
        f"listed entries: {totals['listed_entries']}",
        f"omitted entries: {totals['omitted_entries']}",
        f"shallow bytes: {totals['shallow_bytes']}",
        f"owner marker schema: {plan['owner_marker_contract']['schema']}",
        "by group:",
    ]
    for group, count in totals["by_group"].items():
        lines.append(f"  {group}: {count}")
    lines.append("by owner marker status:")
    for status, count in totals["by_owner_marker_status"].items():
        lines.append(f"  {status}: {count}")
    lines.append("by cleanup safety:")
    for safety, count in totals["by_cleanup_safety"].items():
        lines.append(f"  {safety}: {count}")
    if plan["warnings"]:
        lines.append("warnings:")
        for warning in plan["warnings"]:
            lines.append(f"  - {warning}")
    lines.append("entries:")
    for entry in plan["entries"]:
        lines.append(
            f"  - {entry['path']} [{entry['kind']}, group={entry['group']}, "
            f"owner={entry['owner_hint']}, marker={entry['owner_marker_status']}, "
            f"safety={entry['cleanup_safety']}, age_seconds={entry['age_seconds']}]"
        )
    lines.append(plan["operator_note"])
    return "\n".join(lines)


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Read-only scratch cleanup inventory planner.",
        epilog=(
            "Owner marker contract: directories may contain .pi-agent-target.json "
            f"with schema={OWNER_MARKER_SCHEMA}, optional agent_name/project_key/"
            "purpose/created_at/expires_at, and optional active=false. Active "
            "future markers block cleanup advice; missing or malformed markers "
            "remain unknown ownership."
        ),
    )
    parser.add_argument(
        "--root",
        action="append",
        dest="roots",
        help="scratch root to scan; may be repeated; defaults to /tmp and /data/tmp/recur_agent_cargo",
    )
    parser.add_argument(
        "--pattern",
        action="append",
        dest="patterns",
        help="fnmatch pattern for top-level entries; may be repeated",
    )
    parser.add_argument(
        "--min-age-hours",
        type=float,
        default=0.0,
        help="only include entries at least this old",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=200,
        help="maximum entries to list; use -1 for all",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON instead of text")
    parser.add_argument("--self-test", action="store_true", help="run fixture-backed self-test")
    return parser.parse_args(argv)


def run_self_test() -> int:
    with TemporaryDirectory(prefix="pi-scratch-cleanup-plan-") as tmp:
        root = Path(tmp)
        now_epoch = datetime.now(timezone.utc).timestamp()
        future = datetime.fromtimestamp(now_epoch + 3600, timezone.utc)
        past = datetime.fromtimestamp(now_epoch - 3600, timezone.utc)

        (root / "franken_engine_alpha").mkdir()
        (root / "franken_engine_active").mkdir()
        (root / "franken_engine_active" / ".pi-agent-target.json").write_text(
            json.dumps(
                {
                    "schema": OWNER_MARKER_SCHEMA,
                    "agent_name": "VioletBear",
                    "project_key": "/data/projects/RecurAgent",
                    "purpose": "cargo target cache",
                    "created_at": utc_now_iso(),
                    "expires_at": future.isoformat(),
                }
            ),
            encoding="utf-8",
        )
        (root / "franken_engine_expired").mkdir()
        (root / "franken_engine_expired" / ".pi-agent-target.json").write_text(
            json.dumps(
                {
                    "schema": OWNER_MARKER_SCHEMA,
                    "agent_name": "GreenLake",
                    "expires_at": past.isoformat(),
                }
            ),
            encoding="utf-8",
        )
        (root / "franken_engine_inactive").mkdir()
        (root / "franken_engine_inactive" / ".pi-agent-target.json").write_text(
            json.dumps(
                {
                    "schema": OWNER_MARKER_SCHEMA,
                    "agent_name": "BlueLake",
                    "active": False,
                }
            ),
            encoding="utf-8",
        )
        (root / "franken_engine_malformed").mkdir()
        (root / "franken_engine_malformed" / ".pi-agent-target.json").write_text(
            "{not-json",
            encoding="utf-8",
        )
        (root / "franken_node_beta").write_text("beta", encoding="utf-8")
        (root / "recur_agent_gamma").mkdir()
        (root / "ignore_me").write_text("ignored", encoding="utf-8")
        os.symlink(root / "franken_node_beta", root / "franken_engine_link")

        plan = build_plan(
            roots=[root],
            patterns=("franken*", "recur_agent*"),
            min_age_seconds=0,
            allowed_roots=(root,),
            entry_limit=10,
        )

        assert plan["schema"] == SCHEMA
        assert plan["destructive_actions_executed"] is False
        assert plan["delete_apply_mode_available"] is False
        assert plan["approval_required_for_cleanup"] is True
        assert plan["owner_marker_contract"]["schema"] == OWNER_MARKER_SCHEMA
        assert patterns_for_root(Path(DEFAULT_CARGO_ROOT), DEFAULT_PATTERNS) == ("*",)
        assert patterns_for_root(Path("/tmp"), DEFAULT_PATTERNS) == DEFAULT_PATTERNS
        assert plan["totals"]["matched_entries"] == 8
        assert plan["totals"]["by_group"]["franken_engine"] == 6
        assert plan["totals"]["by_group"]["franken_node"] == 1
        assert plan["totals"]["by_group"]["recur_agent"] == 1
        assert plan["totals"]["by_owner_marker_status"]["active"] == 1
        assert plan["totals"]["by_owner_marker_status"]["expired"] == 1
        assert plan["totals"]["by_owner_marker_status"]["inactive"] == 1
        assert plan["totals"]["by_owner_marker_status"]["malformed"] == 1
        assert plan["totals"]["by_owner_marker_status"]["missing"] == 2
        assert plan["totals"]["by_owner_marker_status"]["not_applicable"] == 2
        assert any(entry["kind"] == "symlink" for entry in plan["entries"])
        assert all(Path(entry["path"]).exists() for entry in plan["entries"])
        by_name = {Path(entry["path"]).name: entry for entry in plan["entries"]}
        assert by_name["franken_engine_active"]["owner_marker_agent"] == "VioletBear"
        assert by_name["franken_engine_active"]["review_action"] == "do_not_delete_active_owner_marker"
        assert by_name["franken_engine_expired"]["cleanup_safety"] == "expired_or_inactive_marker_manual_review"
        assert by_name["franken_engine_inactive"]["owner_marker_status"] == "inactive"
        assert by_name["franken_engine_malformed"]["review_action"] == "manual_review_malformed_owner_marker"
        assert by_name["franken_engine_alpha"]["cleanup_safety"] == "unknown_owner_fail_closed"
        assert by_name["franken_node_beta"]["owner_marker_status"] == "not_applicable"

        limited = build_plan(
            roots=[root],
            patterns=("franken*",),
            min_age_seconds=0,
            allowed_roots=(root,),
            entry_limit=1,
        )
        assert limited["totals"]["listed_entries"] == 1
        assert limited["totals"]["omitted_entries"] == 6

        refused = build_plan(
            roots=[Path("/not-allowed-fixture")],
            patterns=("franken*",),
            min_age_seconds=0,
            allowed_roots=(root,),
            entry_limit=10,
        )
        assert refused["totals"]["matched_entries"] == 0
        assert refused["warnings"]

    print("Scratch cleanup planner self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        return run_self_test()

    if args.min_age_hours < 0:
        print("ERROR: --min-age-hours must be non-negative", file=sys.stderr)
        return 2
    if args.limit < -1:
        print("ERROR: --limit must be -1 or greater", file=sys.stderr)
        return 2

    roots = [Path(root) for root in (args.roots or DEFAULT_ROOTS)]
    patterns = tuple(args.patterns or DEFAULT_PATTERNS)
    allowed_roots = tuple(Path(root) for root in DEFAULT_ROOTS)
    min_age_seconds = int(args.min_age_hours * 3600)
    plan = build_plan(
        roots=roots,
        patterns=patterns,
        min_age_seconds=min_age_seconds,
        allowed_roots=allowed_roots,
        entry_limit=args.limit,
    )

    if args.json:
        print(json.dumps(plan, indent=2, sort_keys=True))
    else:
        print(render_text(plan))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
