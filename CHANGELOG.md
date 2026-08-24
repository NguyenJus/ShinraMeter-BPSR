# Changelog

All notable changes to ShinraMeter-BPSR are documented here. This project is
a fan-made, unofficial tool for Blue Protocol: Star Resonance and is not
affiliated with the game's publisher.

## v0.2.5

### Added

- The header dropdown gains a "Restart packet capture" item. When the meter
  stops updating while the game is still running, it re-anchors capture in
  place, so relaunching the app or re-entering the instance is no longer the
  only way out.
- A saved fight now has the same per-skill breakdown a live one does.
  Right-clicking a player row in a historical encounter lists that player's
  skills instead of reporting that none were recorded. Encounters already on
  disk are kept as they are and simply have no skill rows.

### Changed

- The overlay's default opacity is translucent again. A fresh install, and
  "Reset to defaults", had been coming up fully opaque ever since
  transparency moved onto the opacity slider.
- Pinning the overlay now locks resizing as well as moving. A pinned window
  can no longer be pulled out of shape by its edges or corners.
- The meter watches its own throughput and warns in the log when packets keep
  arriving but nothing reaches it, then re-anchors capture by itself after
  three minutes of that. A capture pipeline that has died outright now raises
  an error banner instead of looking like a quiet fight.

### Fixed

- Making the skill breakdown window narrow collided its column headers into
  unreadable text. It can no longer be sized below the width every column's
  full label needs.
- A boss that simply vanished — no death packet, no zero-HP sync — left the
  fight running on into the next pull. A tracked boss that disappears at low
  health shortly after being hit now ends the fight, timed at the last damage
  rather than at the moment it vanished.
- The opacity slider in the settings dropdown was a fixed narrow width; it
  now fills its row like the items around it.
- The settings dropdown scrolls when an expanded Columns list makes it taller
  than the screen, instead of clipping and getting stuck.

## v0.2.4

### Added

- The header dropdown gains a "Reset to defaults" item. It sizes the overlay
  back to five player rows and puts the opacity back to its default. The
  tray's own window reset is unchanged.
- The header dropdown gains an "Export logs" item, which opens a Save As
  dialog and writes the current and previous log files out as a single file
  — one less thing to hunt for in AppData when reporting a bug.
- Right-clicking a player row in a saved encounter now opens the skill
  breakdown window, the same as a live row does.

### Changed

- Encounter boundaries now follow the game's own dungeon state and objective
  progress wherever the server reports them, rather than being inferred from
  the damage stream alone. Entering a dungeon starts a fresh fight, reaching
  the end screen ends one, and clearing one boss of a multi-boss raid starts
  the next fight in place. A session that never receives any of it behaves
  exactly as before.
- A dungeon's boss is named before the pull from a built-in list of
  single-boss dungeons, instead of being learned from whatever was last
  fought there and remembered on disk. A dungeon that isn't on the list shows
  no name until a boss is actually hit, and the "Forget learned bosses"
  dropdown item is gone along with the guessing it existed to undo.
- The skill breakdown window was measured against the reference and now
  matches it: a taller header with a larger class icon, its own column-header
  band, larger skill icons, roomier rows, and a selected tab that hugs its
  label rather than filling the whole strip.

### Fixed

- The meter could stop updating partway through a raid and stay that way
  until the instance was left and re-entered. A packet the game never resends
  made reassembly throw away the very data it had just recovered onto, so
  nothing reached the meter again for the rest of the session.
- Killing one boss of a multi-boss raid left the timer running, the encounter
  unsaved, and the next boss's damage piling into the dead boss's rows.
- In a long pull with battle rezzes the meter would freeze for about a minute
  and then zero itself, because it counted anyone who had died at any point
  as still being down. It now asks whether the party is down right now.
- Wiping and then re-pulling something the meter doesn't recognize as a boss
  left the timer frozen and everything after it dropped, with no way back
  short of zoning or resetting by hand. The hold now lifts a minute after the
  wipe.
- Being bounced to a checkpoint or lobby immediately after a wipe cleared the
  wiped attempt's rows and death counts before they could be read.
- The skill breakdown window could not be resized, its list stopped scrolling
  after any drag inside it, and its close button drew as an empty square. All
  three work now, and the list has a visible scroll bar.
- The Share button captured the historical encounter list when that was the
  view on screen, rather than any DPS rows; it is greyed out there now. With
  a saved encounter open, the screenshot also cut off the last row or two.

## v0.2.3

### Added

- Every row in the skill breakdown window now leads with the skill's own
  icon. A skill with no icon of its own gets a plain disc in its place, so
  the column always lines up.
- The header's button cluster gains a History button, next to Share and
  Reset. It replaces the "History" item that used to live in the settings
  dropdown, and stays greyed out until there is something to browse.

### Changed

- The skill breakdown window can be resized, and it keeps whatever size it
  was left at for as long as it stays open. It also no longer forces itself
  above every other window just because it was opened, and its header shows
  a larger class icon.
- Right-clicking a row whose skill breakdown is already open now raises that
  window and brings it to the front, instead of appearing to do nothing.
- Class and Imagine icons in each row are larger, the ring around a max-tier
  Imagine is a thinner amber line rather than a thick yellow one, and the
  bracketed Ability Score / Season Strength suffix after a player's name is
  drawn smaller so it reads as secondary to the name.
- The opacity slider covers the full range and both ends mean what they say:
  0% is genuinely transparent and 100% is genuinely solid. It used to stop
  at a 20% floor, and even at its top setting the panel was never fully
  opaque. Row text, header icons, and pill glyphs stay fully visible at
  every setting, so the overlay can always be seen and dragged.
- The pin and click-through toggles moved out of the button cluster and onto
  the title row, in their own pill beside the dropdown chevron.

### Fixed

- The Share button produced nothing on Windows: the copied image was
  effectively invisible when pasted, and any failure was silent. The
  screenshot now pastes as a solid image, and a copy that fails says so.
- Click-through did not actually pass clicks to the game underneath. Input
  now reaches the game, while the click-through button itself stays
  clickable so the mode can be switched back off.
- Pinning the overlay did not stop it being dragged. A pinned overlay no
  longer moves, the cursor says so on hover, and pinning mid-drag stops the
  move already in progress. Resizing a pinned overlay is still allowed.
- A failed screenshot copy showed an error that stayed on the header for the
  rest of the session. It now clears itself after a moment.
- The History button, when there is nothing to browse, is now properly
  reported as disabled to screen readers instead of merely looking greyed
  out.
- Entering a different dungeon left the previous instance's players on
  screen alongside the new party's. The roster is cleared on the way in, and
  so is leftover enemy state, so a new instance can no longer inherit the
  previous dungeon's still-living boss as its own.
- A fight cut short by moving to a different dungeon is now recorded with
  its own reason instead of being logged as a server change.

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
  freeze for review, and nothing the boss does afterwards — its health bar
  refilling, its swings, an AoE clipping an add on the run back — restarts
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
- The header could name a fight other than the one on screen — in
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
