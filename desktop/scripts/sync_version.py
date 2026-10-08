#!/usr/bin/env python3
"""Sync the desktop shell's version with ``deeptutor/__version__.py``.

The desktop app has one source of truth for its version — the same
``deeptutor/__version__.py`` the Python package, the web badge and the release
workflows already use. This script writes it into the places Cargo and Tauri
read, and ``--check`` fails when they have drifted, which is what the release
guard test calls.

Usage:

    python3 desktop/scripts/sync_version.py          # write
    python3 desktop/scripts/sync_version.py --check  # verify only (exit 1 on drift)
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[2]
VERSION_FILE = REPO_ROOT / "deeptutor" / "__version__.py"

# (file, pattern with a single capture group around the version)
TARGETS: tuple[tuple[Path, str], ...] = (
    (
        REPO_ROOT / "desktop" / "src-tauri" / "Cargo.toml",
        r'(?m)^(version\s*=\s*")([^"]+)(")',
    ),
    (
        REPO_ROOT / "desktop" / "src-tauri" / "tauri.conf.json",
        r'(\n\s*"version"\s*:\s*")([^"]+)(")',
    ),
    (
        REPO_ROOT / "desktop" / "plugins" / "tauri-plugin-deeptutor" / "Cargo.toml",
        r'(?m)^(version\s*=\s*")([^"]+)(")',
    ),
)


def source_version() -> str:
    text = VERSION_FILE.read_text(encoding="utf-8")
    match = re.search(r'__version__\s*=\s*["\']([^"\']+)["\']', text)
    if not match:
        raise SystemExit(f"could not read __version__ from {VERSION_FILE}")
    return match.group(1)


def current_version(path: Path, pattern: str) -> str:
    match = re.search(pattern, path.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(f"could not find a version in {path}")
    return match.group(2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="report drift instead of fixing it")
    args = parser.parse_args()

    expected = source_version()
    drifted: list[str] = []
    for path, pattern in TARGETS:
        actual = current_version(path, pattern)
        if actual == expected:
            print(f"ok   {path.relative_to(REPO_ROOT)}: {actual}")
            continue
        if args.check:
            drifted.append(f"{path.relative_to(REPO_ROOT)}: {actual} != {expected}")
            continue
        text = path.read_text(encoding="utf-8")
        updated = re.sub(pattern, lambda match: f"{match.group(1)}{expected}{match.group(3)}", text)
        path.write_text(updated, encoding="utf-8")
        print(f"set  {path.relative_to(REPO_ROOT)}: {actual} -> {expected}")

    if drifted:
        print("version drift:", file=sys.stderr)
        for line in drifted:
            print(f"  {line}", file=sys.stderr)
        print("run: python3 desktop/scripts/sync_version.py", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
