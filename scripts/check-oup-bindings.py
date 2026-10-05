#!/usr/bin/env python3
"""Check the built C ABI and generated Python binding against this checkout."""

import argparse
import difflib
import os
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def normalized_generated(text):
    # Match the documented generator cleanup; never rewrite committed bindings.
    return "\n".join(line.rstrip(" \t") for line in text.splitlines()).rstrip("\n") + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library-dir", type=Path, default=ROOT / "target/debug")
    args = parser.parse_args()
    library_dir = args.library_dir.resolve()
    suffix = ".dylib" if sys.platform == "darwin" else ".so"
    library = library_dir / f"libra_uniffi{suffix}"
    bindgen = library_dir / "uniffi-bindgen"
    if sys.platform.startswith("win"):
        library = library_dir / "ra_uniffi.dll"
        bindgen = bindgen.with_suffix(".exe")
    with tempfile.TemporaryDirectory(prefix="ra-binding-parity-") as output:
        subprocess.run([str(bindgen), "generate", "--library", str(library),
                        "--language", "python", "--no-format", "--out-dir", output],
                       check=True, cwd=ROOT, timeout=120)
        expected = (ROOT / "crates/ra-uniffi/bindings/python/ra.py").read_text()
        actual = normalized_generated((Path(output) / "ra.py").read_text())
        if expected != actual:
            sys.stderr.writelines(difflib.unified_diff(
                expected.splitlines(keepends=True), actual.splitlines(keepends=True),
                fromfile="committed ra.py", tofile="generated ra.py"))
            raise SystemExit("Python bindings are stale; regenerate using the README instructions")
    subprocess.run([os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                    "-fsyntax-only", "-I", str(ROOT / "crates/ra-ffi/include"),
                    str(ROOT / "crates/ra-ffi/tests/header_contract.c")], check=True, timeout=30)
    subprocess.run([sys.executable, str(ROOT / "crates/ra-uniffi/tests/incomplete_bindings.py"),
                    "--library-dir", str(library_dir)], check=True, timeout=120)
    print("PASS: generated Python parity, C declarations and actual C/Python runtime contracts")


if __name__ == "__main__":
    main()
