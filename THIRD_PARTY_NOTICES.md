# Third-party notices

`shinra-bpsr` redistributes the following third-party software inside its
executable.

## WinDivert 2.2.2

- Files: `crates/capture/vendor/windivert/WinDivert.dll`,
  `crates/capture/vendor/windivert/WinDivert64.sys`
- Upstream: <https://reqrypt.org/windivert.html> — <https://github.com/basil00/WinDivert>
- Copyright (c) Basil Nikolopoulos
- License: dual-licensed under your choice of the GNU Lesser General Public
  License version 3, or the GNU General Public License version 2. The full
  text as shipped by the upstream project is in
  `crates/capture/vendor/windivert/LICENSE`.

Both files are the official pre-built, digitally signed x64 release binaries,
embedded verbatim and unmodified. They are unpacked to
`%LOCALAPPDATA%\shinra-bpsr\windivert\<version>\` at runtime and the library
is loaded dynamically from there, so the WinDivert components remain separate,
replaceable files: substituting your own build of the same version requires
only replacing them in that directory.

`shinra-bpsr` itself is licensed GPL-3.0-only (see `LICENSE`), with which
WinDivert's LGPL-3.0 option is compatible.

## BPSR-ZDPS class icons

- Files: `crates/app/assets/classes/*.png`
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS>
- License: MIT
- Embedded verbatim (compiled into the executable via `include_bytes!` in
  `crates/app/src/icons.rs`), unmodified, at build time.

## ShinraMeter toolbar icons

- Files: `crates/app/assets/icons/*.png`
- Upstream: <https://github.com/neowutran/ShinraMeter> (`master` branch,
  `resources/img/`). The branch matters: `master` carries these as PNGs, while
  the unreleased `mvvm_refactor_wip` branch replaces them with SVG path data in
  `DamageMeter.UI/Resources/SVG.xaml`. These files are the PNGs; the SVG-derived
  glyphs are covered by the three sections below.
- License: MIT
- Embedded verbatim (compiled into the executable via `include_bytes!` in
  `crates/app/src/icons.rs`), unmodified, at build time. `settings.png` is
  upstream's `config.png`, renamed here to match what it is used for in this
  project — no pixel data changed.

## ShinraMeter encounter emblem

- Files: `crates/app/assets/icons/svg/emblem.svg`,
  `crates/app/assets/icons/glyphs/emblem.png`
- Upstream: <https://github.com/neowutran/ShinraMeter> (`mvvm_refactor_wip`
  branch, `DamageMeter.UI/Resources/SVG.xaml`, the `Svg.HPBar` `PathGeometry`)
- License: MIT
- The horned-beast-in-a-diamond mark behind the header's encounter name. Its
  path data is reproduced verbatim; the only change is the SVG wrapper needed to
  rasterize it — a computed `viewBox` (the WPF `PathGeometry` declares none),
  an explicit `fill="#ffffff"` so this project's own paint-time tint controls
  the color, and `fill-rule="nonzero"` (WPF `PathGeometry` defaults to EvenOdd,
  which renders this path incorrectly). The PNG is generated from that SVG by
  `scripts/rasterize-icons.sh` and committed alongside it.
- This mark is ShinraMeter's own artwork, unlike the Material glyphs below.

## Google Material Symbols

- Files: `crates/app/assets/icons/svg/{timer,speed,heart,cloud_off,check}.svg`
  and the correspondingly named PNGs under
  `crates/app/assets/icons/glyphs/`
- Upstream: <https://github.com/google/material-design-icons> — the
  `timer`, `speed`, `favorite`, `cloud_off` and `check` symbols
- Copyright (c) Google LLC
- License: Apache License 2.0, which is compatible with this project's
  GPL-3.0-only licence.
- Reached this project via ShinraMeter's `mvvm_refactor_wip`
  `DamageMeter.UI/Resources/SVG.xaml`, which carries them as bare
  `PathGeometry` resources with no attribution of its own. The path data is
  unmistakably Material Symbols — the 960-unit `0 -960 960 960` coordinate
  system and `M400-840q…` compression are that project's export format — so the
  grant recorded here is Google's, not ShinraMeter's. Embedded verbatim; the
  only changes are the SVG wrapper described above.

## Material Design Icons (Pictogrammers)

- Files: `crates/app/assets/icons/svg/{skull,mouse_off}.svg` and the
  correspondingly named PNGs under `crates/app/assets/icons/glyphs/`
- Upstream: <https://github.com/Templarian/MaterialDesign> — the `skull` and
  `cursor-default-click-outline` (struck-through) icons
- Copyright (c) Austin Andrews and the Pictogrammers contributors
- License: Apache License 2.0, which is compatible with this project's
  GPL-3.0-only licence.
- Same provenance note as the Material Symbols above: they arrive via
  ShinraMeter's uncredited `SVG.xaml`, but their 24-unit `0 0 24 24` coordinate
  system identifies them as Material Design Icons, so the grant recorded here is
  Pictogrammers', not ShinraMeter's. Embedded verbatim, with the same SVG
  wrapper.

## Monster and scene name tables

- Files: `crates/meter/data/MonsterName.json`, `crates/meter/data/SceneName.json`
  (from `resonance-logs/resonance-logs`, GPL-3.0) and
  `crates/meter/data/MonsterNameCrowdsource.json` (from `winjwinj/bpsr-logs`,
  GPL-3.0). See "Boss-monster id list" below for
  `crates/meter/data/MonsterNameBoss.json`.
- Upstream: <https://github.com/resonance-logs/resonance-logs>,
  <https://github.com/winjwinj/bpsr-logs>
- License: GPL-3.0, matching this project's own licence
- Not shipped as files: compiled into `crates/meter/src/tables.rs` by
  `scripts/gen-name-tables.py` at development time, then built into the
  executable as Rust source.

## Boss-monster id list

- Files: `crates/meter/data/MonsterNameBoss.json`
- Upstream: <https://github.com/winjwinj/bpsr-logs> (also shipped identically
  by <https://github.com/resonance-logs/resonance-logs>)
- License: GPL-3.0, matching this project's own licence
- Compiled into the `BOSS_MONSTER_IDS` constant and `is_boss_monster` in
  `crates/meter/src/tables.rs` by `scripts/gen-name-tables.py`, the same as
  the tables above. Used to gate the top-bar encounter name to boss fights
  only (issue #42): `Meter::recompute_boss` has no boss/trash classification
  of its own, so this id set is consulted at display time to decide whether
  the resolved target is a real boss worth naming, rather than a large trash
  pull.
