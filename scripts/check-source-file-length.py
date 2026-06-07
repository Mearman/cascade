#!/usr/bin/env python3
"""Fail if any production Rust source file exceeds the line cap.

Large files are hard to navigate and review; once a module grows past the cap it
should be split into focused submodules (see scripts/extract_test_modules.py for
moving test modules out, and the `mod foo;` + sibling-file pattern for source).

Test files are exempt: a `*_tests.rs` sibling or anything under a `tests/`
directory legitimately holds many independent cases. The cap targets the
production logic a reader has to hold in their head.

Run from the repo root: `python3 scripts/check-source-file-length.py`
Exits non-zero (listing offenders) if any file is over the cap.
"""
import os
import sys

# A production Rust source file over this many lines should be componentised.
# Chosen as a round ceiling above the current largest file after the v0.1
# componentisation; lower it as files shrink, never silently raise it.
MAX_LINES = 2500

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = os.path.join(ROOT, "crates")


def is_exempt(path: str) -> bool:
    base = os.path.basename(path)
    if base.endswith("_tests.rs"):
        return True
    parts = path.replace("\\", "/").split("/")
    return "tests" in parts


def main() -> int:
    offenders = []
    for dirpath, _dirs, files in os.walk(CRATES):
        for name in files:
            if not name.endswith(".rs"):
                continue
            full = os.path.join(dirpath, name)
            if is_exempt(full):
                continue
            with open(full, "r", encoding="utf-8", errors="replace") as f:
                n = sum(1 for _ in f)
            if n > MAX_LINES:
                offenders.append((n, os.path.relpath(full, ROOT)))

    if offenders:
        offenders.sort(reverse=True)
        print(f"Source files exceed the {MAX_LINES}-line cap; split them into submodules:")
        for n, rel in offenders:
            print(f"  {n:>6}  {rel}")
        return 1
    print(f"OK: no production Rust source file exceeds {MAX_LINES} lines.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
