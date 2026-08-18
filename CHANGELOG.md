# Changelog

All notable changes to ShinraMeter-BPSR are documented here. This project is
a fan-made, unofficial tool for Blue Protocol: Star Resonance and is not
affiliated with the game's publisher.

## v0.2.0

### Added

- Share button to copy an encounter screenshot to the clipboard, and a Reset
  button; stats now stay on screen after a fight ends instead of clearing
  immediately.
- Each player's two equipped Imagines now show in their row.
- Death count column, system tray minimize, and a header collapse control.
- Class and Imagine icons now load from external asset files instead of
  being baked into the executable, and can be updated or removed without a
  new build.

### Changed

- Overlay now opens transparent immediately instead of flat gray until the
  first resize.
- Reworked header layout: spacing, emblem, and timer geometry now match the
  reference design; damage-share bars keep a visible minimum width and the
  Share screenshot is cropped to populated rows.
- Boss and summoned-monster identification now reads the game's own
  classification data instead of a hardcoded id list, so more encounters are
  correctly named.
- Restyled the whole meter (fonts, colors, icons) to match the current
  Shinra reference design, and renamed the project to ShinraMeter-BPSR.
- The Columns submenu no longer has an accidental-hover trap, and the
  redundant collapse toggle was removed.
- Diagnostic logging is more informative: unknown packet attributes and
  encounter transitions are now logged.
- Packet-inspection diagnostics (`SHINRA_INSPECT`) are now opt-in instead of
  opt-out, since the dumps they produce can contain other players' names.
  Set `SHINRA_INSPECT=1` to enable them; by default nothing is written.

### Fixed

- The Share screenshot never actually reached the clipboard.
- A drag-resize could run away and freeze the app.
- An oversize window resize could kill the overlay.
- The tray icon showed Windows' stock icon instead of the app's icon.
- The overlay's scene/boss decoding used the wrong packet field and could
  misidentify or blank out the current area.

## v0.1.0

Initial tagged release.
