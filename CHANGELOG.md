# Changelog

All notable changes to ShinraMeter-BPSR are documented here. This project is
a fan-made, unofficial tool for Blue Protocol: Star Resonance and is not
affiliated with the game's publisher.

## v0.2.2

### Added

- An opacity slider in the settings dropdown, so the overlay's background
  and border can be made translucent while the rows stay fully legible. The
  chosen level persists across restarts.
- Ability Score and Season Strength, when enabled, now appear inline after
  the player's name as a dimmed bracketed suffix instead of each reserving
  its own stat column.
- Click-through and always-on-top toggles in the header's button cluster.
  Click-through lets mouse input pass to the game underneath the overlay,
  and both choices survive a restart. The inert queue gauge that used to
  occupy that space is gone.
- Each Imagine's tier now shows in a tooltip on hover, and an Imagine at
  max tier is drawn with a gold ring.
- A "Check for updates" item in the settings dropdown. It compares the
  running build against the latest published release and links to it when a
  newer one exists. Nothing is ever checked in the background.
- The overlay now remembers its size across restarts, not just its
  position.
- Each dungeon's final boss is now remembered between sessions, so the
  banner can name it from the start of a run instead of having to relearn
  it every launch.

### Changed

- Damage and DPS now read to about four significant figures, so a number
  like 12345 shows as 12.34K instead of 12.3K.
- Party members' names, classes, and Ability Scores now come from the
  game's own party-roster message, so every member is named regardless of
  how far away they are, and names are preloaded when entering a dungeon or
  raid. Rows no longer sit as `Player 12345` until that player acts.
- Class and Imagine icons are compiled back into the executable. They no
  longer load from loose files beside the app, so a missing, partially
  extracted, or quarantined asset folder can't leave the meter without
  icons.
- The banner names the dungeon's final boss and holds that name for the
  whole run, instead of following whatever monster is currently being hit.

### Fixed

- After a party wipe the boss's continued attacks kept the fight running
  indefinitely, diluting every row's DPS with dead time. Only player
  activity extends a fight now.
- A wipe now ends the attempt the same way a boss kill does: the rows
  freeze for review, and nothing the boss does afterwards - its health bar
  refilling, its swings, an AoE clipping an add on the run back - restarts
  the clock or clears the board. The next fight begins when a player
  damages a recognized boss again.
- A long immunity or mechanic window no longer ended a live pull. While a
  damaged boss is still alive in a dungeon, the idle timeout is suspended,
  so the meter no longer froze mid-fight and then wiped every row when the
  party resumed.
- Hitting trash could hijack the meter: an unrecognized enemy could take
  over the boss slot while a real boss was still up, and it could trigger
  the health-rollback reset. Both are now limited to recognized bosses.
- A boss changing phase no longer ends the fight. While another living,
  damaged boss remains in the encounter, the meter keeps running instead of
  freezing on the phase that died.
- Zoning no longer wiped the meter. Changing map or reconnecting keeps the
  totals on screen for reading and screenshotting; they clear at the start
  of the next real fight.
- Paradox-Calamity Remnant's three raid bosses were treated as phases of a
  single fight, so fighting a second one continued the first one's numbers
  instead of starting fresh. They are separate fights again.
- The header could name a fight other than the one on screen - in
  particular, a raid's remembered boss was only ever a guess, since a raid
  lets you pick a different one of its three bosses without leaving the
  instance. The header now names the fight it is actually showing.
- The Share screenshot captured the Share button's own hover highlight, and
  the header's background wash stopped short of the first player row.

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
