#!/usr/bin/env python3
"""Prepare the committed skill-row icons under `crates/app/assets/skills/` (issue #192).

Dev-time only: the PNGs are committed and `include_bytes!`d via the generated
`crates/app/src/skill_icons.rs`, so a normal build never runs this. Re-run it
only to refresh or extend the icon set, then re-run
`scripts/gen-skill-icons.py` and commit both.

The sources are the skill icons committed in the BPSR-ZDPS reference tracker.
They are game-client-derived assets — see `THIRD_PARTY_NOTICES.md` for the basis
on which this project redistributes them. Only the basenames actually named by the vendored icon tables
(`crates/meter/data/SkillOverridesIcons.json` and, since issue #247,
`crates/meter/data/SkillTableIcons.json` — the `Icon` halves of BPSR-ZDPS's
curated skill overrides and of the full client skill table, produced by
`scripts/gen-name-tables.py`) are copied, so the redistributed set stays as small as the feature needs — the
same discipline the Imagine icons follow.

Upstream splits the icons across two sibling directories, and the `Icon` field
does not say which: `Data/Images/Skills/` holds the regular skill icons and
`Data/Images/Skills_Imagines/` the Imagine ones, and referenced basenames land
in both. Both are searched, in that order.

To refresh:

    git clone --depth 1 https://github.com/Blue-Protocol-Source/BPSR-ZDPS /tmp/zdps
    python3 scripts/prep-skill-icons.py /tmp/zdps/BPSR-ZDPS/Data/Images
    python3 scripts/gen-skill-icons.py

The same two transforms `scripts/prep-imagine-icons.py` applies are applied
here, by importing its helpers rather than restating them — a downscale to
`SIZE` (Lanczos) and a circular alpha mask, so a painted icon and the blank
placeholder the row degrades to read as the same shape. `SIZE` matches the Imagine
set's 48: the breakdown row draws these at 24pt, so 48px is still 2x at 100%
scaling, and the sources (~256px) are far larger than needed. 64px was measured
and rejected — it costs 1.7 MB of embedded binary against 48px's 1.1 MB for
detail no supported scaling factor resolves.

Icons named by the vendored table but absent from both source directories are
reported and skipped, not fatal: the table is deliberately allowed to name icons
this project does not ship, and an unshipped basename degrades to the blank
placeholder at draw time.
"""

import importlib
import json
import pathlib
import sys

from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Both vendored icon layers (issue #247): the curated overrides and the full
# client table that backfills them. `bpsr_meter::tables::skill_icon` is their
# union, so the committed PNG set has to be their union too.
TABLES = (
    ROOT / "crates" / "meter" / "data" / "SkillOverridesIcons.json",
    ROOT / "crates" / "meter" / "data" / "SkillTableIcons.json",
)
OUT = ROOT / "crates" / "app" / "assets" / "skills"

# `prep-imagine-icons.py` is not an importable identifier (the dash), hence
# `import_module`. Reused rather than copied so the two vendored icon sets can
# never drift into two different maskings of the same source art.
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
_imagine = importlib.import_module("prep-imagine-icons")

# Rendered at 24pt (see `SKILL_ICON_SIZE` in `crates/app/src/ui.rs`).
SIZE = 48

# Searched in order; the first hit wins. Upstream's `Icon` field names a basename
# with no directory, so which of these a given icon lives in is discovered, not
# declared.
SOURCE_DIRS = ("Skills", "Skills_Imagines")


def main() -> None:
    _imagine._self_test()

    if len(sys.argv) != 2:
        sys.exit(f"usage: {pathlib.Path(sys.argv[0]).name} <Data/Images dir>")
    images = pathlib.Path(sys.argv[1])
    if not images.is_dir():
        sys.exit(f"not a directory: {images}")

    wanted = sorted(
        {v for t in TABLES for v in json.loads(t.read_text(encoding="utf-8")).values()}
    )

    found, missing = {}, []
    for name in wanted:
        for sub in SOURCE_DIRS:
            src = images / sub / f"{name}.png"
            if src.is_file():
                found[name] = src
                break
        else:
            missing.append(name)

    if not found:
        sys.exit(f"no referenced icon found under {images} — wrong directory?")

    OUT.mkdir(parents=True, exist_ok=True)
    for name, src in found.items():
        _imagine.prepare(Image.open(src), SIZE).save(OUT / f"{name}.png", optimize=True)

    # Drop anything left from a previous run whose basename is no longer
    # referenced, so the committed set never accumulates icons the meter cannot
    # draw. Mirrors `prep-imagine-icons.py`'s same-purpose sweep.
    stale = [p for p in OUT.glob("*.png") if p.stem not in found]
    for path in stale:
        path.unlink()

    print(f"wrote {len(found)} icon(s) to {OUT}")
    if stale:
        print(f"removed {len(stale)} stale icon(s): {', '.join(p.stem for p in stale)}")
    if missing:
        print(
            f"{len(missing)} referenced icon(s) not present upstream, skipped "
            f"(they degrade to the blank placeholder): {', '.join(missing)}"
        )


if __name__ == "__main__":
    main()
