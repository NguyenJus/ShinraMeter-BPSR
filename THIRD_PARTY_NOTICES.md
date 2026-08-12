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
