"""A pack must not claim a version its payload does not contain.

The 2026-09-24 Phase 2 run built a "1.6.11" pack whose wheel was still 1.6.10:
`--app-version` only renames the archive, while the wheel comes from
`deeptutor/__version__.py`. Every checksum and the whole delta pipeline stayed
internally consistent, so nothing complained — the mistake would only surface
as "the update installed and nothing changed".
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "desktop" / "pack" / "build_pack.py"
SPEC = importlib.util.spec_from_file_location("build_pack", SCRIPT)
assert SPEC and SPEC.loader
build_pack = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build_pack)


def test_wheel_version_is_read_from_the_filename() -> None:
    assert build_pack.wheel_version(Path("deeptutor-1.6.11-py3-none-any.whl")) == "1.6.11"
    with pytest.raises(SystemExit):
        build_pack.wheel_version(Path("something-else.whl"))


def test_a_mislabelled_pack_is_refused() -> None:
    with pytest.raises(SystemExit) as error:
        build_pack.assert_wheel_matches_label(Path("deeptutor-1.6.10-py3-none-any.whl"), "1.6.11")
    assert "bump deeptutor/__version__.py" in str(error.value)


def test_a_correct_label_passes() -> None:
    build_pack.assert_wheel_matches_label(Path("deeptutor-1.6.11-py3-none-any.whl"), "1.6.11")
