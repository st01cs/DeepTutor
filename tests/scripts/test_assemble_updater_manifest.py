"""The shell update manifest is the one document a broken release bricks.

These tests pin the parts that would fail *silently* in production: a platform
key the updater never looks up, a mixed-version merge, and an empty signature
file (a signature the updater would reject only after a user tried to update).
"""

from __future__ import annotations

import importlib.util
import base64
import json
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "desktop" / "scripts" / "assemble_updater_manifest.py"
SPEC = importlib.util.spec_from_file_location("assemble_updater_manifest", SCRIPT)
assert SPEC and SPEC.loader
manifest_script = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manifest_script)

# `tauri build` writes the .sig file base64-encoded; the updater decodes it back
# to a minisign document before verifying.
MINISIGN = (
    "untrusted comment: signature from tauri secret key\n"
    "RWQdummybase64payload==\n"
    "trusted comment: timestamp:1\tfile:x\tversion:1.6.11\n"
    "AAAA\n"
)
SIGNATURE = base64.b64encode(MINISIGN.encode("utf-8")).decode("ascii")


def write_artifact(tmp_path: Path, name: str) -> Path:
    artifact = tmp_path / name
    artifact.write_bytes(b"fake bundle")
    signature = tmp_path / f"{name}.sig"
    signature.write_text(SIGNATURE, encoding="utf-8")
    return artifact


def run_manifest(*args: str) -> int:
    return manifest_script.main(list(args))


def test_first_platform_writes_a_complete_manifest(tmp_path: Path) -> None:
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    manifest = tmp_path / "latest.json"

    assert (
        run_manifest(
            "--manifest",
            str(manifest),
            "--version",
            "1.6.11",
            "--platform",
            "darwin-aarch64",
            "--artifact",
            str(artifact),
            "--signature",
            f"{artifact}.sig",
            "--base-url",
            "https://github.com/st01cs/DeepTutor/releases/download/v1.6.11/",
            "--pub-date",
            "2026-09-24T12:00:00Z",
        )
        == 0
    )

    payload = json.loads(manifest.read_text(encoding="utf-8"))
    assert payload["version"] == "1.6.11"
    assert payload["pub_date"] == "2026-09-24T12:00:00Z"
    entry = payload["platforms"]["darwin-aarch64"]
    assert entry["url"].endswith("/DeepTutor.app.tar.gz")
    # Stored exactly as the updater expects it: base64 around a minisign file.
    assert (
        base64.b64decode(entry["signature"]).decode("utf-8").startswith("untrusted comment:")
    )


def test_merging_keeps_the_platforms_already_published(tmp_path: Path) -> None:
    manifest = tmp_path / "latest.json"
    mac = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    run_manifest(
        "--manifest", str(manifest), "--version", "1.6.11",
        "--platform", "darwin-aarch64", "--artifact", str(mac),
        "--signature", f"{mac}.sig", "--base-url", "https://example.test/v1.6.11/",
    )
    windows = write_artifact(tmp_path, "DeepTutor_1.6.11_x64-setup.exe")
    run_manifest(
        "--manifest", str(manifest), "--version", "1.6.11",
        "--platform", "windows-x86_64", "--artifact", str(windows),
        "--signature", f"{windows}.sig", "--base-url", "https://example.test/v1.6.11/",
    )

    payload = json.loads(manifest.read_text(encoding="utf-8"))
    assert set(payload["platforms"]) == {"darwin-aarch64", "windows-x86_64"}
    # A second runner must not rewrite the first one's timestamp into chaos.
    assert payload["platforms"]["darwin-aarch64"]["url"].endswith("DeepTutor.app.tar.gz")


def test_a_second_version_is_refused(tmp_path: Path) -> None:
    manifest = tmp_path / "latest.json"
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    run_manifest(
        "--manifest", str(manifest), "--version", "1.6.11",
        "--platform", "darwin-aarch64", "--artifact", str(artifact),
        "--signature", f"{artifact}.sig", "--base-url", "https://example.test/",
    )
    with pytest.raises(SystemExit, match="refusing to mix"):
        run_manifest(
            "--manifest", str(manifest), "--version", "1.6.12",
            "--platform", "windows-x86_64", "--artifact", str(artifact),
            "--signature", f"{artifact}.sig", "--base-url", "https://example.test/",
        )


def test_an_unknown_platform_is_rejected_before_it_is_written(tmp_path: Path) -> None:
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    with pytest.raises(SystemExit):
        run_manifest(
            "--manifest", str(tmp_path / "latest.json"), "--version", "1.6.11",
            "--platform", "macos-arm64",  # the *pack* name, not the updater key
            "--artifact", str(artifact), "--signature", f"{artifact}.sig",
            "--base-url", "https://example.test/",
        )


def test_a_missing_or_empty_signature_is_refused(tmp_path: Path) -> None:
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    empty = tmp_path / "empty.sig"
    empty.write_text("\n", encoding="utf-8")
    with pytest.raises(SystemExit, match="empty"):
        run_manifest(
            "--manifest", str(tmp_path / "latest.json"), "--version", "1.6.11",
            "--platform", "darwin-aarch64", "--artifact", str(artifact),
            "--signature", str(empty), "--base-url", "https://example.test/",
        )
    with pytest.raises(SystemExit, match="not found"):
        run_manifest(
            "--manifest", str(tmp_path / "latest.json"), "--version", "1.6.11",
            "--platform", "darwin-aarch64", "--artifact", str(artifact),
            "--signature", str(tmp_path / "nope.sig"), "--base-url", "https://example.test/",
        )


def test_an_unencoded_minisign_file_is_refused(tmp_path: Path) -> None:
    """A raw minisign document is *not* what the updater reads from latest.json."""
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    raw = tmp_path / "raw.sig"
    raw.write_text(MINISIGN, encoding="utf-8")
    with pytest.raises(SystemExit, match="base64"):
        run_manifest(
            "--manifest", str(tmp_path / "latest.json"), "--version", "1.6.11",
            "--platform", "darwin-aarch64", "--artifact", str(artifact),
            "--signature", str(raw), "--base-url", "https://example.test/",
        )


def test_a_missing_artifact_is_refused(tmp_path: Path) -> None:
    signature = tmp_path / "x.sig"
    signature.write_text(SIGNATURE, encoding="utf-8")
    with pytest.raises(SystemExit, match="artifact not found"):
        run_manifest(
            "--manifest", str(tmp_path / "latest.json"), "--version", "1.6.11",
            "--platform", "darwin-aarch64",
            "--artifact", str(tmp_path / "absent.app.tar.gz"),
            "--signature", str(signature), "--base-url", "https://example.test/",
        )


def test_notes_come_from_the_release_body(tmp_path: Path) -> None:
    artifact = write_artifact(tmp_path, "DeepTutor.app.tar.gz")
    notes = tmp_path / "notes.md"
    notes.write_text("## 1.6.11\n\n- tray, notifications, deep links\n", encoding="utf-8")
    manifest = tmp_path / "latest.json"
    run_manifest(
        "--manifest", str(manifest), "--version", "1.6.11",
        "--platform", "darwin-aarch64", "--artifact", str(artifact),
        "--signature", f"{artifact}.sig", "--base-url", "https://example.test/",
        "--notes-file", str(notes),
    )
    payload = json.loads(manifest.read_text(encoding="utf-8"))
    assert payload["notes"].startswith("## 1.6.11")
