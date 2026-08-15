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
  `DamageMeter.UI/Resources/SVG.xaml`. These files are the PNGs.
- License: MIT
- Embedded verbatim (compiled into the executable via `include_bytes!` in
  `crates/app/src/icons.rs`), unmodified, at build time. `settings.png` is
  upstream's `config.png` and `clock.png` is upstream's `historic.png`,
  renamed here to match what each is used for in this project — no pixel
  data changed.

## ShinraMeter application icon

- Files: `crates/app/assets/shinra.ico`
- Upstream: <https://github.com/neowutran/ShinraMeter> (`resources/img/shinra.ico`)
- License: MIT
- Embedded as resource id 2 by `crates/app/shinra-bpsr.rc`, and used as the
  executable's Explorer/taskbar icon and the notification-area icon. It is
  upstream's 神羅 mark, carried over deliberately: this project is a Blue
  Protocol port of ShinraMeter and shares its name. The upstream .ico ships
  16x16, 48x48, and 256x256 frames; a 32x32 frame was added here, downscaled
  from the original 256x256 image with Lanczos resampling, so Windows surfaces
  that render at 32px (Explorer's "Large icons" view, Alt+Tab, some tray DPI
  scalings) don't have to stretch a neighbouring frame. No other pixel data
  was changed.

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
