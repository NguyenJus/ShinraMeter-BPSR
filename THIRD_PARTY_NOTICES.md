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

## Monster and scene name tables

- Files: `crates/meter/data/MonsterName.json`, `crates/meter/data/SceneName.json`
  (from `resonance-logs/resonance-logs`, GPL-3.0) and
  `crates/meter/data/MonsterNameCrowdsource.json` (from `winjwinj/bpsr-logs`,
  GPL-3.0)
- Upstream: <https://github.com/resonance-logs/resonance-logs>,
  <https://github.com/winjwinj/bpsr-logs>
- License: GPL-3.0, matching this project's own licence
- Not shipped as files: compiled into `crates/meter/src/tables.rs` by
  `scripts/gen-name-tables.py` at development time, then built into the
  executable as Rust source.
