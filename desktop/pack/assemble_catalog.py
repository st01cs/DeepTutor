#!/usr/bin/env python3
"""Assemble ``runtime-packs.json`` from per-pack catalog fragments.

`build_pack.py` emits one `<pack_id>.catalog.json` per platform. The shell only
reads the merged catalog, so the release workflow runs this once after the
matrix finishes and uploads the result next to the archives.

Archive URLs default to plain file names (the shell resolves them relative to
the catalog), which is what a GitHub Release with all assets in one place
needs. Pass ``--base-url`` to point somewhere else.

Usage:

    python3 desktop/pack/assemble_catalog.py --dist desktop/pack/dist
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import time

DEFAULT_DIST = Path(__file__).resolve().parent / "dist"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=DEFAULT_DIST)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--base-url", default="", help="prefix for archive URLs")
    args = parser.parse_args()

    # `*.catalog.json` would also match `<pack>.delta.catalog.json`; those
    # describe an incremental path, not a pack, and are merged further down.
    fragments = [
        fragment
        for fragment in sorted(args.dist.glob("*.catalog.json"))
        if not fragment.name.endswith(".delta.catalog.json")
    ]
    if not fragments:
        raise SystemExit(f"no *.catalog.json under {args.dist}")

    packs = []
    for fragment in fragments:
        data = json.loads(fragment.read_text(encoding="utf-8"))
        archive = data["archive"]
        packs.append(
            {
                "pack_id": data["pack_id"],
                "app_version": data["app_version"],
                "platform": data["platform"],
                "url": f"{args.base_url}{archive}" if args.base_url else archive,
                "sha256": data["sha256"],
                "size": data["size"],
                "requires_shell": data.get("requires_shell", ">=1.0.0"),
                "python": data.get("python"),
                "node": data.get("node"),
            }
        )

    # `build_delta.py` writes one fragment per incremental update, naming the
    # pack it upgrades *from*. Attaching it to that pack's entry is what lets an
    # installed shell take the small download when its base matches
    # (`PackInstaller::update_from_catalog`), and the full archive otherwise.
    deltas: dict[str, dict] = {}
    for fragment in sorted(args.dist.glob("*.delta.catalog.json")):
        for entry in json.loads(fragment.read_text(encoding="utf-8")).get("deltas", []):
            archive = entry["archive"]
            deltas[entry["pack_id"]] = {
                "base_pack_id": entry["base_pack_id"],
                "url": f"{args.base_url}{archive}" if args.base_url else archive,
                "sha256": entry["sha256"],
                "size": entry["size"],
            }
    matched = 0
    for pack in packs:
        delta = deltas.get(pack["pack_id"])
        if delta is None:
            continue
        if delta["base_pack_id"] == pack["pack_id"]:
            raise SystemExit(
                f"{pack['pack_id']} would upgrade from itself; check the delta fragment"
            )
        pack["delta"] = delta
        matched += 1
    orphaned = sorted(set(deltas) - {pack["pack_id"] for pack in packs})
    if orphaned:
        # A delta with no pack in this catalog is unusable: the shell has nothing
        # to download for a fresh install, and nothing to fall back to.
        raise SystemExit(f"delta fragment(s) without a matching pack: {', '.join(orphaned)}")

    out = args.out or args.dist / "runtime-packs.json"
    payload = {
        "schema_version": 1,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "packs": packs,
    }
    out.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    platforms = ", ".join(sorted({pack["platform"] for pack in packs}))
    print(
        f"wrote {out} with {len(packs)} pack(s) for {platforms}"
        + (f", {matched} with an incremental path" if matched else "")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
