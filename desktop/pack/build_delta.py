#!/usr/bin/env python3
"""Build an incremental runtime-pack update (a "delta") from two staged packs.

A full runtime pack is ~250 MB compressed / ~760 MB unpacked, but most of that
never changes between two releases: the CPython tree, the Node runtime and
almost every wheel in the venv are byte-identical. What actually changes is the
DeepTutor wheel itself and the web bundle — tens of megabytes at most. Shipping
that (Spike B in the plan) is what turns "every update is a 250 MB download"
into "every update is a small patch applied to the pack you already have".

Outputs, next to the full pack:

* ``<target_pack_id>.delta.tar.gz`` — ``delta.json``, the added/changed files
  under ``files/``, and ``links.json`` when symlinks changed;
* ``<target_pack_id>.delta.catalog.json`` — a fragment ``assemble_catalog.py``
  merges into the matching pack entry as its ``delta``.

The delta is only valid for one exact base pack, so ``delta.json`` records a
digest of the base tree. The applier verifies it before touching anything: a
delta built against a different base is refused rather than mixed in.

Excluded from both the diff and the digest: ``__pycache__``, ``*.pyc``/``*.pyo``
and ``.DS_Store``. Those appear *after* install (Python writes bytecode the first
time it imports, a user browsing the folder can drop a .DS_Store), so including
them would make every delta build fail the base check on a machine that has
actually run the app.

Usage::

    python3 desktop/pack/build_delta.py \
        --base desktop/pack/dist/stage-1.6.10-macos-aarch64 \
        --target desktop/pack/dist/stage-1.6.11-macos-aarch64
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import time

DELTA_SCHEMA_VERSION = 1

# See the module docstring: these are runtime artefacts, not pack content.
EXCLUDED_DIR_NAMES = {"__pycache__"}
EXCLUDED_SUFFIXES = {".pyc", ".pyo"}
EXCLUDED_FILE_NAMES = {".DS_Store"}

# Mirrors `rewrite_prefix`'s size gate in the Rust installer: larger files are
# never rewritten during rehydration, so they are fingerprinted verbatim.
MAX_REWRITE_BYTES = 256 * 1024
# What a file's own pack-root path collapses to when it is fingerprinted. The
# Rust side uses the same token (`pack_tree::PACK_ROOT_TOKEN`).
PACK_ROOT_TOKEN = "<PACK_ROOT>"
SELF_REFERENCING_LINKS = {"venv/bin/python", "venv/bin/python3", "venv/bin/python3.12"}


def log(message: str) -> None:
    print(f"[delta] {message}", flush=True)


def is_excluded(name: str) -> bool:
    return name in EXCLUDED_FILE_NAMES or Path(name).suffix.lower() in EXCLUDED_SUFFIXES


def describe(root: Path, path: Path) -> tuple[str, int, str]:
    """``(kind, size, digest)`` for one entry, in the installer's vocabulary.

    Sizes and hashes are taken over the *fingerprinted* bytes: a small text file
    that mentions its own pack root is digested with that path replaced by the
    token, exactly as an install rewrites it — including the size, because the
    two roots are different lengths.
    """

    if path.is_symlink():
        target = os.readlink(path)
        needle = str(root)
        if needle in target:
            target = target.replace(needle, PACK_ROOT_TOKEN)
        return "l", 0, hashlib.sha256(target.encode()).hexdigest()

    raw = path.read_bytes()
    if len(raw) > MAX_REWRITE_BYTES:
        return "f", len(raw), hashlib.sha256(raw).hexdigest()
    try:
        text = raw.decode()
    except UnicodeDecodeError:
        return "f", len(raw), hashlib.sha256(raw).hexdigest()
    needle = str(root)
    if needle in text:
        text = text.replace(needle, PACK_ROOT_TOKEN)
    encoded = text.encode()
    return "f", len(encoded), hashlib.sha256(encoded).hexdigest()


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def payload_digest(path: Path) -> tuple[int, str]:
    """``(size, sha256)`` of the bytes that actually travel in the archive.

    Deliberately *not* [`describe`]: that one tokenises a pack's own path so two
    trees can be compared, while this must match the file as shipped — the
    installer hashes the extracted bytes before putting them in place.
    """

    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            size += len(chunk)
            digest.update(chunk)
    return size, digest.hexdigest()


def iter_tree(root: Path):
    """Yield ``(relative_path, kind, size, digest)`` sorted by path.

    ``kind`` is ``"f"`` for a regular file and ``"l"`` for a symlink; for a link
    the digest is over the target text and the size is 0. This is the same
    vocabulary the Rust applier uses, and the digest is the thing both sides
    compare, so a mismatch here is a refused update rather than a silent one.
    """

    rows = []
    for directory, dirnames, filenames in os.walk(root):
        here = Path(directory)
        # A symlinked directory is content, not a place to descend into.
        for index in range(len(dirnames) - 1, -1, -1):
            candidate = here / dirnames[index]
            if candidate.is_symlink():
                relative = candidate.relative_to(root).as_posix()
                rows.append((relative, *describe(root, candidate)))
                dirnames.pop(index)
            elif dirnames[index] in EXCLUDED_DIR_NAMES:
                dirnames.pop(index)

        for name in filenames:
            if is_excluded(name):
                continue
            candidate = here / name
            relative = candidate.relative_to(root).as_posix()
            # Mirrors `is_rehydrated_link`: those three links change *form* at
            # install time, so they are not part of the fingerprint at all.
            if relative in SELF_REFERENCING_LINKS:
                continue
            if candidate.is_symlink() or candidate.is_file():
                rows.append((relative, *describe(root, candidate)))
    rows.sort(key=lambda row: row[0])
    return rows


def tree_digest(rows) -> str:
    """Digest over the whole tree listing; the applier recomputes this exactly."""

    digest = hashlib.sha256()
    for relative, kind, size, entry_digest in rows:
        digest.update(f"{relative}\t{kind}\t{size}\t{entry_digest}\n".encode())
    return digest.hexdigest()


def _entry(root: Path, relative: str) -> dict:
    """A delta entry for one shipped file: its raw bytes, not its fingerprint."""

    size, digest = payload_digest(root / relative)
    return {"size": size, "sha256": digest}


def pack_identity(root: Path) -> tuple[str, str, str]:
    manifest_path = root / "manifest.json"
    if not manifest_path.is_file():
        raise SystemExit(f"{root} has no manifest.json (not a staged pack tree)")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    return manifest["pack_id"], manifest["platform"], manifest["app_version"]


def materialise(source: Path, workdir: Path) -> Path:
    """Accept a staged directory or a pack archive.

    The returned path is always absolute: the tree's own path is written *into*
    the pack (shebangs, `pyvenv.cfg`, wheel metadata), and the installer has to
    tokenise exactly those strings. A relative path recorded here would silently
    tokenise nothing — which is how a 108-byte base mismatch showed up on
    2026-09-24 before this was resolved.
    """
    if source.is_dir():
        return source.resolve()
    if not source.is_file():
        raise SystemExit(f"neither a directory nor an archive: {source}")
    destination = workdir / source.name.replace(".tar.gz", "").replace(".tgz", "")
    destination.mkdir(parents=True, exist_ok=True)
    log(f"extracting {source.name}")
    with tarfile.open(source, "r:gz") as handle:
        handle.extractall(destination)
    return destination.resolve()


def build_delta(base_root: Path, target_root: Path, out_dir: Path) -> dict:
    base_rows = iter_tree(base_root)
    target_rows = iter_tree(target_root)
    base = {relative: (kind, size, digest) for relative, kind, size, digest in base_rows}
    target = {relative: (kind, size, digest) for relative, kind, size, digest in target_rows}

    added = [relative for relative in target if relative not in base]
    changed = [
        relative
        for relative, entry in target.items()
        if relative in base and base[relative] != entry
    ]
    removed = sorted(relative for relative in base if relative not in target)

    base_pack_id, platform, _ = pack_identity(base_root)
    target_pack_id, target_platform, app_version = pack_identity(target_root)
    if platform != target_platform:
        raise SystemExit(f"platform mismatch: {platform} vs {target_platform}")

    files = [relative for relative in added + changed if target[relative][0] == "f"]
    links = {
        relative: os.readlink(target_root / relative)
        for relative in added + changed
        if target[relative][0] == "l"
    }
    files.sort()

    delta = {
        "schema_version": DELTA_SCHEMA_VERSION,
        "base_pack_id": base_pack_id,
        "target_pack_id": target_pack_id,
        "platform": platform,
        "app_version": app_version,
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        # Where the two trees were staged. The installer needs these because
        # rehydration only rewrites the paths *inside the venv*: everything else
        # (the CPython tree, the manifest, a wheel's metadata outside site-
        # packages) keeps the build directory forever, so those strings have to
        # be tokenised on the installed copy before it can be compared.
        "base_root": str(base_root),
        "target_root": str(target_root),
        "base_tree_sha256": tree_digest(base_rows),
        "base_file_count": len(base_rows),
        "base_total_size": sum(row[2] for row in base_rows),
        "target_tree_sha256": tree_digest(target_rows),
        "added": [
            {"path": relative, **_entry(target_root, relative)}
            for relative in added
            if target[relative][0] == "f"
        ],
        "changed": [
            {"path": relative, **_entry(target_root, relative)}
            for relative in changed
            if target[relative][0] == "f"
        ],
        "removed": removed,
        "links": [{"path": relative, "target": value} for relative, value in sorted(links.items())],
        "unchanged_file_count": len(target_rows) - len(added) - len(changed),
    }

    out_dir.mkdir(parents=True, exist_ok=True)
    archive = out_dir / f"{target_pack_id}.delta.tar.gz"
    log(
        f"delta {base_pack_id} -> {target_pack_id}: "
        f"+{len(files)} changed, -{len(removed)} removed, {len(links)} links"
    )
    with tarfile.open(archive, "w:gz", compresslevel=6) as handle:
        # The manifest travels under a fixed name so the applier can read it
        # before it trusts anything else in the archive.
        packed = out_dir / ".delta.pack"
        packed.mkdir(parents=True, exist_ok=True)
        manifest_path = packed / "delta.json"
        manifest_path.write_text(
            json.dumps(delta, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        handle.add(manifest_path, arcname="delta.json")
        for relative in files:
            handle.add(target_root / relative, arcname=f"files/{relative}")
        links_path = packed / "links.json"
        links_path.write_text(
            json.dumps(delta["links"], indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        handle.add(links_path, arcname="links.json")
    shutil.rmtree(out_dir / ".delta.pack", ignore_errors=True)

    size = archive.stat().st_size
    digest = sha256_of(archive)
    (archive.parent / f"{archive.name}.sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8"
    )

    fragment = {
        "schema_version": DELTA_SCHEMA_VERSION,
        "deltas": [
            {
                "pack_id": target_pack_id,
                "base_pack_id": base_pack_id,
                "platform": platform,
                "app_version": app_version,
                "archive": archive.name,
                "size": size,
                "sha256": digest,
                "added": len(files),
                "removed": len(removed),
            }
        ],
    }
    fragment_path = out_dir / f"{target_pack_id}.delta.catalog.json"
    fragment_path.write_text(
        json.dumps(fragment, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    log(f"archive {archive.name}: {size / 1e6:.1f} MB, sha256 {digest[:16]}…")
    log(f"catalog fragment: {fragment_path}")
    return delta


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True, help="staged base pack (or its .tar.gz)")
    parser.add_argument("--target", type=Path, required=True, help="staged target pack (or its .tar.gz)")
    parser.add_argument("--out", type=Path, default=None, help="output directory")
    args = parser.parse_args()

    out_dir = args.out or (args.target.parent if args.target.parent.name == "dist" else Path(__file__).parent / "dist")
    with tempfile.TemporaryDirectory(prefix="deeptutor-delta-") as workdir:
        work = Path(workdir)
        base_root = materialise(args.base, work)
        target_root = materialise(args.target, work)
        delta = build_delta(base_root, target_root, out_dir)
    print(json.dumps({k: delta[k] for k in ("base_pack_id", "target_pack_id", "added", "changed", "removed", "unchanged_file_count")}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
