#!/usr/bin/env python3
"""Assemble Tauri's ``latest.json`` from per-platform updater artifacts.

The shell asks exactly one URL (``plugins.updater.endpoints``) for a document
that maps every platform to an artifact and the minisign signature of that
artifact:

.. code-block:: json

    {
      "version": "1.6.11",
      "notes": "…",
      "pub_date": "2026-09-24T12:00:00Z",
      "platforms": {
        "darwin-aarch64": {"signature": "…", "url": "https://…/DeepTutor.app.tar.gz"},
        "darwin-x86_64": {"signature": "…", "url": "https://…/DeepTutor.app.tar.gz"},
        "windows-x86_64": {"signature": "…", "url": "https://…/DeepTutor_x64-setup.exe"}
      }
    }

``tauri build`` writes one ``<artifact>.sig`` next to each updater bundle but no
manifest, so this script merges one platform per invocation — the release job
runs it once per runner after downloading that runner's artifacts. Merging (as
opposed to generating the whole document in one place) is what keeps the
manifest reproducible from the runners themselves: each platform's signature
travels with the artifact that was signed on that platform.

Usage::

    assemble_updater_manifest.py --manifest latest.json --version 1.6.11 \\
        --platform darwin-aarch64 --artifact DeepTutor.app.tar.gz \\
        --signature DeepTutor.app.tar.gz.sig \\
        --base-url https://github.com/o/r/releases/download/v1.6.11/ \\
        [--notes-file NOTES.md]
"""

from __future__ import annotations

import argparse
import base64
import binascii
from datetime import datetime, timezone
import json
from pathlib import Path
import sys

# The keys the updater looks up (see tauri-plugin-updater's `json_target`).
# Anything else in `platforms` is ignored at runtime, so a typo here would be a
# silently dead channel — hence the explicit allow-list.
KNOWN_PLATFORMS: tuple[str, ...] = (
    "darwin-aarch64",
    "darwin-x86_64",
    "windows-x86_64",
    "linux-x86_64",
)

def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, help="latest.json to write (merged when present)")
    parser.add_argument("--version", required=True, help="release version this manifest describes")
    parser.add_argument("--platform", required=True, choices=KNOWN_PLATFORMS)
    parser.add_argument("--artifact", required=True, help="artifact path (basename becomes the URL)")
    parser.add_argument(
        "--signature",
        required=True,
        help="path to the .sig file produced by `tauri build`",
    )
    parser.add_argument(
        "--base-url",
        required=True,
        help="download prefix for the artifacts, usually the release asset URL",
    )
    parser.add_argument("--notes-file", help="release notes to embed")
    parser.add_argument(
        "--pub-date",
        default=None,
        help="RFC3339 timestamp; defaults to now (UTC)",
    )
    return parser.parse_args(argv)


def read_signature(path: Path) -> str:
    """Read a `.sig` file, refusing anything the updater could not verify.

    Tauri writes the signature **base64-encoded**: the manifest value is the
    file's text, and the updater decodes it to a minisign document before
    parsing. So the sanity check is a round trip — decode, then look for the
    `untrusted comment:` header — which catches an empty file, a truncated
    upload, or a raw (un-encoded) minisign file pasted by hand.
    """
    if not path.is_file():
        raise SystemExit(f"signature file not found: {path}")
    text = path.read_text(encoding="utf-8").strip()
    if not text:
        raise SystemExit(f"{path} is empty; the update channel would be dead on arrival")
    try:
        decoded = base64.b64decode(text, validate=True).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError, ValueError) as error:
        raise SystemExit(f"{path} is not a base64 minisign signature: {error}") from error
    lines = [line for line in decoded.splitlines() if line.strip()]
    if len(lines) < 2 or not lines[0].startswith("untrusted comment:"):
        raise SystemExit(
            f"{path} does not decode to a minisign signature (expected a comment line and a key)"
        )
    return text


def artifact_url(base_url: str, artifact: Path) -> str:
    if base_url.startswith("file://"):
        return f"{base_url.rstrip('/')}/{artifact.name}"
    return f"{base_url.rstrip('/')}/{artifact.name}"


def load_manifest(path: Path, version: str) -> dict:
    if not path.exists():
        # Exactly the shape `tauri-plugin-updater` parses; no extra fields, so a
        # future strict reader cannot start rejecting our manifests.
        return {
            "version": version,
            "notes": "",
            "pub_date": "",
            "platforms": {},
        }
    payload = json.loads(path.read_text(encoding="utf-8"))
    existing = payload.get("version")
    if existing != version:
        raise SystemExit(
            f"{path} already describes {existing!r}, refusing to mix it with {version!r}"
        )
    payload.setdefault("platforms", {})
    return payload


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    manifest_path = Path(args.manifest)
    artifact = Path(args.artifact)
    if not artifact.is_file():
        raise SystemExit(f"artifact not found: {artifact} (did the bundle step produce it?)")

    signature = read_signature(Path(args.signature))
    payload = load_manifest(manifest_path, args.version)

    notes = payload.get("notes") or ""
    if args.notes_file:
        notes = Path(args.notes_file).read_text(encoding="utf-8").strip()
    pub_date = args.pub_date or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    payload.update(
        {
            "version": args.version,
            "notes": notes,
            "pub_date": pub_date,
            "platforms": {
                **payload.get("platforms", {}),
                args.platform: {
                    "signature": signature,
                    "url": artifact_url(args.base_url, artifact),
                },
            },
        }
    )

    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"manifest {manifest_path}: {args.platform} -> {payload['platforms'][args.platform]['url']}"
    )
    print(f"platforms: {', '.join(sorted(payload['platforms']))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
