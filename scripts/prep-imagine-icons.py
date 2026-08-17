#!/usr/bin/env python3
"""Prepare the committed Imagine icons under `crates/app/assets/imagines/` (issue #33).

Dev-time only: the PNGs are committed and `include_bytes!`d by
`crates/app/src/icons.rs`, so a normal build never runs this. Re-run it only to
refresh or extend the icon set, then commit the regenerated PNGs.

The sources are the Imagine skill icons committed in the BPSR-ZDPS reference
tracker. They are game-client-derived assets — see `THIRD_PARTY_NOTICES.md` for
the basis on which this project redistributes them, and note that only the icons
actually referenced by `crates/app/data/imagine_table.json` are copied, so the
redistributed set stays as small as the feature needs.

To refresh:

    git clone --depth 1 https://github.com/Blue-Protocol-Source/BPSR-ZDPS /tmp/zdps
    python3 scripts/prep-imagine-icons.py /tmp/zdps/BPSR-ZDPS/Data/Images/Skills_Imagines

Three transforms are applied, each deliberate:

* **Subset** — only the icons named by the curated table. The reference ships 86;
  the 5 unreferenced ones are not redistributed.
* **Downscale** to `SIZE`, Lanczos. The row renders these at 14pt, so the 124x124
  sources are far larger than needed; this cuts ~2.6 MB to ~200 KB of embedded
  binary. `THIRD_PARTY_NOTICES.md` records that the assets are resampled rather
  than verbatim.
* **Circular alpha mask**, so a filled slot and the blank-circle placeholder read
  as the same shape. Done here rather than at draw time because egui cannot
  circular-clip an image without a custom mesh.
"""

import json
import pathlib
import sys

from PIL import Image, ImageChops, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parent.parent
TABLE = ROOT / "crates" / "app" / "data" / "imagine_table.json"
OUT = ROOT / "crates" / "app" / "assets" / "imagines"

# Rendered at 14pt (see `IMAGINE_ICON_SIZE` in `crates/app/src/ui.rs`); 48px
# stays crisp past 300% display scaling without paying for the 124px sources.
SIZE = 48

# The mask is built at this multiple of `SIZE` and downsampled, which
# antialiases the circle edge. Drawing an ellipse straight into a 48px mask
# leaves visibly stepped edges.
_MASK_SUPERSAMPLE = 4


def circular_mask(size: int, supersample: int = _MASK_SUPERSAMPLE) -> Image.Image:
    """An `L`-mode circle mask of `size`x`size`, antialiased via supersampling."""
    big = size * supersample
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).ellipse((0, 0, big - 1, big - 1), fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def prepare(src: Image.Image, size: int = SIZE) -> Image.Image:
    """Downscale `src` to `size` and intersect its alpha with a circular mask.

    Intersects rather than replaces: an icon that is already transparent in a
    corner must stay transparent, so the mask can only ever remove opacity.
    """
    icon = src.convert("RGBA").resize((size, size), Image.LANCZOS)
    icon.putalpha(ImageChops.darker(icon.getchannel("A"), circular_mask(size)))
    return icon


def _self_test() -> None:
    """Fast synthetic checks run on every invocation.

    There is no Python test harness in this repo (no pytest config, no CI job
    running Python — see `scripts/` and `.github/workflows/`), so this hard-fails
    inline rather than living in an untested, never-run test file. Mirrors
    `scripts/gen-name-tables.py`'s `_self_test`.
    """
    mask = circular_mask(16)
    assert mask.size == (16, 16)
    assert mask.getpixel((8, 8)) == 255, "circle centre must be fully opaque"
    assert mask.getpixel((0, 0)) == 0, "circle corner must be fully transparent"

    # A fully-opaque square must come out circular...
    opaque = Image.new("RGBA", (64, 64), (255, 0, 0, 255))
    out = prepare(opaque, 16)
    assert out.size == (16, 16)
    assert out.getchannel("A").getpixel((8, 8)) == 255
    assert out.getchannel("A").getpixel((0, 0)) == 0

    # ...and the mask must only ever remove opacity, never add it.
    clear = Image.new("RGBA", (64, 64), (255, 0, 0, 0))
    assert prepare(clear, 16).getchannel("A").getpixel((8, 8)) == 0, (
        "masking must not make a transparent source opaque"
    )


def main() -> None:
    _self_test()

    if len(sys.argv) != 2:
        sys.exit(f"usage: {pathlib.Path(sys.argv[0]).name} <Skills_Imagines dir>")
    src_dir = pathlib.Path(sys.argv[1])
    if not src_dir.is_dir():
        sys.exit(f"not a directory: {src_dir}")

    table = json.loads(TABLE.read_text())
    wanted = sorted({row["icon"] for row in table["imagines"]})

    missing = [n for n in wanted if not (src_dir / f"{n}.png").is_file()]
    if missing:
        sys.exit(f"{len(missing)} icon(s) missing from {src_dir}: {', '.join(missing)}")

    OUT.mkdir(parents=True, exist_ok=True)
    for name in wanted:
        prepare(Image.open(src_dir / f"{name}.png")).save(
            OUT / f"{name}.png", optimize=True
        )

    # Drop anything left from a previous run whose id was since removed from the
    # curated table, so the committed set never drifts past what is referenced.
    stale = [p for p in OUT.glob("*.png") if p.stem not in set(wanted)]
    for p in stale:
        p.unlink()

    total = sum(p.stat().st_size for p in OUT.glob("*.png"))
    print(
        f"wrote {len(wanted)} icons to {OUT} at {SIZE}x{SIZE} "
        f"({total / 1024:.0f} KiB total)"
        + (f"; removed {len(stale)} stale" if stale else "")
    )


if __name__ == "__main__":
    main()
