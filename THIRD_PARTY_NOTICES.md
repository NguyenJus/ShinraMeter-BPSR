# Third-party notices

`ShinraMeter-BPSR` redistributes the following third-party software inside its
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
`%LOCALAPPDATA%\ShinraMeter-BPSR\windivert\<version>\` at runtime and the library
is loaded dynamically from there, so the WinDivert components remain separate,
replaceable files: substituting your own build of the same version requires
only replacing them in that directory.

`ShinraMeter-BPSR` itself is licensed GPL-3.0-only (see `LICENSE`), with which
WinDivert's LGPL-3.0 option is compatible.

## egui-winit 0.36.1

- Files: `third_party/egui-winit/`
- Upstream: <https://github.com/emilk/egui/tree/main/crates/egui-winit>
- Copyright (c) 2018-2021 Emil Ernerfeldt and the egui contributors
- License: dual-licensed under your choice of the MIT license or the Apache
  License version 2.0. The full text of both, copied from the upstream
  repository at tag 0.36.1, is in `third_party/egui-winit/LICENSE-MIT` and
  `third_party/egui-winit/LICENSE-APACHE`. Either option is compatible with
  this project's GPL-3.0-only licence.
- A vendored fork (issue #89), compiled into the executable: the workspace's
  `[patch.crates-io]` entry redirects both this project's and eframe's
  dependency on `egui-winit` to this copy. It is upstream 0.36.1 verbatim apart
  from one addition, the `set_no_redirection_bitmap` opt-in in `src/lib.rs` and
  the `WS_EX_NOREDIRECTIONBITMAP` it sets on the window winit creates — plus
  two dead manifest keys dropped, described at the top of
  `third_party/egui-winit/Cargo.toml`.

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

- Files: `crates/app/assets/icons/svg/{skull,mouse_off,share}.svg` and the
  correspondingly named PNGs under `crates/app/assets/icons/glyphs/`
- Upstream: <https://github.com/Templarian/MaterialDesign> — the `skull`,
  `cursor-default-click-outline` (struck-through), and `export` icons
- Copyright (c) Austin Andrews and the Pictogrammers contributors
- License: Apache License 2.0, which is compatible with this project's
  GPL-3.0-only licence.
- Same provenance note as the Material Symbols above: they arrive via
  ShinraMeter's uncredited `SVG.xaml`, but their 24-unit `0 0 24 24` coordinate
  system identifies them as Material Design Icons, so the grant recorded here is
  Pictogrammers', not ShinraMeter's. Embedded verbatim, with the same SVG
  wrapper.

## ShinraMeter application icon

- Files: `crates/app/assets/shinra.ico`
- Upstream: <https://github.com/neowutran/ShinraMeter> (`resources/img/shinra.ico`)
- License: MIT
- Embedded as resource id 2 by `crates/app/ShinraMeter-BPSR.rc`, and used as the
  executable's Explorer/taskbar icon and the notification-area icon. It is
  upstream's 神羅 mark, carried over deliberately: this project is a Blue
  Protocol port of ShinraMeter and shares its name. The upstream .ico ships
  16x16, 48x48, and 256x256 frames; a 32x32 frame was added here, downscaled
  from the original 256x256 image with Lanczos resampling, so Windows surfaces
  that render at 32px (Explorer's "Large icons" view, Alt+Tab, some tray DPI
  scalings) don't have to stretch a neighbouring frame. No other pixel data
  was changed.

## Monster and scene name tables (community)

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
- These are the hand-checked, player-facing names, and the generator ranks them
  *above* the BPSR-ZDPS tables below wherever the two disagree on an id.

## Monster and scene name tables (BPSR-ZDPS)

- Files: `crates/meter/data/MonsterTableNames.json`,
  `crates/meter/data/SceneTableNames.json`,
  `crates/meter/data/DungeonsTableNames.json`
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> —
  `BPSR-ZDPS/Data/MonsterTable.json`, `SceneTable.json`, `DungeonsTable.json`
- Copyright (c) 2025 Blue-Protocol-Source
- License: **MIT — note this differs from the GPL-3.0 of the community tables
  above.** MIT is the more permissive of the two and imposes only the
  attribution this section provides, so combining the two in a GPL-3.0-only
  work is fine; the combined result is governed by this project's GPL-3.0-only
  licence. BPSR-ZDPS states its licence once, at the repository root, with no
  carve-out for `Data/`.
- **Filtered, not verbatim.** Upstream these three files are roughly 6.5 MB,
  0.6 MB and 1.1 MB of full row data — stats, drop tables, AI references and
  asset paths. What is vendored here is only the `{id: name}` projection of
  each (about 97 KB, 19 KB and 17 KB), produced by `filter_id_names` in
  `scripts/gen-name-tables.py`; nothing else from those rows is copied into
  this repository. Re-run `scripts/gen-name-tables.py --refresh` to reproduce
  the filtering from upstream.
- Added by issue #36 to backfill the ids the community tables do not cover:
  they take `monster_name` from 216 to 3,094 ids and `scene_name` from 340 to
  605, so far fewer encounters fall back to a raw `Monster #40010` /
  `Scene #1201` placeholder in the header.
- Not shipped as files: like the community tables, they are compiled into
  `crates/meter/src/tables.rs` at development time and built into the
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
- Deliberately left as the small hand-checked list by issue #36's backfill.
  That backfill grew `monster_name` more than tenfold, which grew the
  population of trash `recompute_boss` can resolve a name for — so this gate
  became more load-bearing, not less. The BPSR-ZDPS `MonsterTable.json` has no
  trustworthy substitute for it: its `MonsterRank` field marks elites and
  minibosses alongside raid bosses.
