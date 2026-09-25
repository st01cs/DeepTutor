"""The merged catalog is the only file the shell reads for runtime updates.

It has to describe three things consistently: which packs exist, which of them
an installed copy may reach incrementally, and what to fall back to when the
base does not match. A delta that loses its pack — or claims to upgrade from
itself — is a broken update channel, so both are refused here.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "desktop" / "pack" / "assemble_catalog.py"
SPEC = importlib.util.spec_from_file_location("assemble_catalog", SCRIPT)
assert SPEC and SPEC.loader
assemble_catalog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assemble_catalog)


def write_fragment(directory: Path, pack_id: str, version: str) -> None:
    (directory / f"{pack_id}.catalog.json").write_text(
        json.dumps(
            {
                "pack_id": pack_id,
                "app_version": version,
                "platform": "macos-aarch64",
                "archive": f"{pack_id}.tar.gz",
                "size": 257078810,
                "sha256": "a" * 64,
            }
        )
        + "\n",
        encoding="utf-8",
    )


def write_delta_fragment(directory: Path, pack_id: str, base_pack_id: str) -> None:
    (directory / f"{pack_id}.delta.catalog.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "deltas": [
                    {
                        "pack_id": pack_id,
                        "base_pack_id": base_pack_id,
                        "archive": f"{pack_id}.delta.tar.gz",
                        "size": 2200000,
                        "sha256": "b" * 64,
                    }
                ],
            }
        )
        + "\n",
        encoding="utf-8",
    )


def test_a_delta_is_attached_to_its_target_pack(tmp_path: Path, monkeypatch) -> None:
    write_fragment(tmp_path, "1.6.10-macos-aarch64", "1.6.10")
    write_fragment(tmp_path, "1.6.11-macos-aarch64", "1.6.11")
    write_delta_fragment(tmp_path, "1.6.11-macos-aarch64", "1.6.10-macos-aarch64")
    monkeypatch.setattr(
        "sys.argv", ["assemble_catalog.py", "--dist", str(tmp_path), "--base-url", "https://x/"]
    )
    assert assemble_catalog.main() == 0

    payload = json.loads((tmp_path / "runtime-packs.json").read_text(encoding="utf-8"))
    by_id = {pack["pack_id"]: pack for pack in payload["packs"]}
    delivered = by_id["1.6.11-macos-aarch64"]
    assert delivered["delta"]["base_pack_id"] == "1.6.10-macos-aarch64"
    assert delivered["delta"]["url"] == "https://x/1.6.11-macos-aarch64.delta.tar.gz"
    # The full archive stays available: it is the fallback for a mismatched base.
    assert delivered["url"] == "https://x/1.6.11-macos-aarch64.tar.gz"
    assert "delta" not in by_id["1.6.10-macos-aarch64"]


def test_a_delta_without_its_pack_is_refused(tmp_path: Path, monkeypatch) -> None:
    write_fragment(tmp_path, "1.6.10-macos-aarch64", "1.6.10")
    write_delta_fragment(tmp_path, "1.6.11-macos-aarch64", "1.6.10-macos-aarch64")
    monkeypatch.setattr("sys.argv", ["assemble_catalog.py", "--dist", str(tmp_path)])
    with pytest.raises(SystemExit, match="without a matching pack"):
        assemble_catalog.main()


def test_a_delta_from_itself_is_refused(tmp_path: Path, monkeypatch) -> None:
    write_fragment(tmp_path, "1.6.11-macos-aarch64", "1.6.11")
    write_delta_fragment(tmp_path, "1.6.11-macos-aarch64", "1.6.11-macos-aarch64")
    monkeypatch.setattr("sys.argv", ["assemble_catalog.py", "--dist", str(tmp_path)])
    with pytest.raises(SystemExit, match="upgrade from itself"):
        assemble_catalog.main()
