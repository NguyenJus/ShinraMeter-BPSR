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
- Basis: these are game-client-derived assets, obtained via BPSR-ZDPS.
  BPSR-ZDPS's own `LICENSE` is MIT, but that grant covers ZDPS's source code,
  not art extracted from the game client — ZDPS applying MIT to those files
  does not make the art theirs to license, so this is not recorded as an MIT
  grant. Redistributed here on the inferred basis that the game's developers
  have publicly endorsed open-source meters and have not objected to other
  meters shipping the same data. To be explicit: no permission was requested
  or granted directly, by either the game's developers or BPSR-ZDPS.
- Shipped verbatim and unmodified, compiled into the executable via
  `include_bytes!` (issue #123).
- Takedown: these assets will be removed promptly on request from the rights
  holder. To request removal, open a GitHub issue on this repository. For an
  already-distributed release, the takedown is deleting
  `crates/app/assets/classes/` from the source tree, rebuilding, and
  re-releasing — the icons are compiled in, so a directory deletion in an
  extracted copy is not enough on its own.

## BPSR-ZDPS Imagine icons

- Files: `crates/app/assets/imagines/*.png` — 81 of the upstream 86. The
  remaining 5 are not referenced by any entry in
  `crates/app/data/imagine_table.json` and are deliberately not
  redistributed: only the icons the meter actually draws are shipped,
  keeping the footprint to what the feature needs.
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS>
  (`Data/Images/Skills_Imagines/`)
- Basis: same as the class icons above — game-client-derived assets, obtained
  via BPSR-ZDPS, redistributed on the inferred basis that the game's
  developers have publicly endorsed open-source meters and have not objected
  to other meters shipping the same data. No permission was requested or
  granted directly, by either the game's developers or BPSR-ZDPS.
- Modified: downscaled from the upstream 124x124 to 48x48 with Lanczos
  resampling, then alpha-masked to a circle, by
  `scripts/prep-imagine-icons.py`. Resampled with stated provenance, not
  verbatim — the same register the egui-winit note above uses.
- Shipped compiled into the executable via `include_bytes!` (issue #123).
  The file names and bytes are generated into `IMAGINE_ICON_FILES` and
  `IMAGINE_ICON_BYTES` in `crates/app/src/imagines.rs` by
  `scripts/gen-imagine-table.py`.
- The id/name table these icons are keyed against —
  `crates/app/data/imagine_table.json` (hand-curated) and the `imagines.rs`
  it generates via `scripts/gen-imagine-table.py` — is likewise derived from
  BPSR-ZDPS's `Data/SkillTable.json`, under the same basis as the icons.
- Takedown: these assets will be removed promptly on request from the rights
  holder. To request removal, open a GitHub issue on this repository. For an
  already-distributed release, the takedown is deleting
  `crates/app/assets/imagines/` from the source tree, rebuilding, and
  re-releasing — the icons are compiled in, so a directory deletion in an
  extracted copy is not enough on its own.

## BPSR-ZDPS skill icons

- Files: `crates/app/assets/skills/*.png` — 387 of the 446 icon basenames
  referenced by `crates/meter/data/SkillOverridesIcons.json` and
  `crates/meter/data/SkillTableIcons.json` together. The 59 remaining
  references have no image upstream at all: some are not asset names (upstream
  stores the prose "From Shield Combo talent" in one row's `Icon` field), the
  rest name art the client ships in an atlas BPSR-ZDPS does not extract
  (`talent_skill_*`, `weapon_iruna_*`). Only the icons the meter can actually
  draw are shipped, the same footprint discipline the Imagine icons above
  follow — the 348 + 86 upstream images are not bulk-redistributed.
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS>
  (`Data/Images/Skills/` and `Data/Images/Skills_Imagines/`; the `Icon` field
  does not say which of the two an icon lives in, so both are searched)
- Basis: same as the class and Imagine icons above — game-client-derived
  assets, obtained via BPSR-ZDPS, redistributed on the inferred basis that the
  game's developers have publicly endorsed open-source meters and have not
  objected to other meters shipping the same data. No permission was requested
  or granted directly, by either the game's developers or BPSR-ZDPS.
- Modified: downscaled to 48x48 with Lanczos resampling, then alpha-masked to
  a circle, by `scripts/prep-skill-icons.py` — which reuses
  `scripts/prep-imagine-icons.py`'s transforms rather than restating them.
  Resampled with stated provenance, not verbatim, the same register the
  Imagine-icon note above uses.
- Shipped compiled into the executable via `include_bytes!` (issue #123's
  reasoning). The file names and bytes are generated into `SKILL_ICON_FILES`
  and `SKILL_ICON_BYTES` in `crates/app/src/skill_icons.rs` by
  `scripts/gen-skill-icons.py`.
- The id -> icon tables these are keyed against,
  `crates/meter/data/SkillOverridesIcons.json` and
  `crates/meter/data/SkillTableIcons.json`, and the `skill_icon` function they
  generate in `crates/meter/src/tables.rs`, are covered by the BPSR-ZDPS game
  table section below, under that section's MIT grant.
- Takedown: these assets will be removed promptly on request from the rights
  holder. To request removal, open a GitHub issue on this repository. For an
  already-distributed release, the takedown is deleting
  `crates/app/assets/skills/` from the source tree, rebuilding, and
  re-releasing — the icons are compiled in, so a directory deletion in an
  extracted copy is not enough on its own.

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

- Files: `crates/app/assets/icons/svg/{timer,speed,heart}.svg`
  and the correspondingly named PNGs under
  `crates/app/assets/icons/glyphs/`
- Upstream: <https://github.com/google/material-design-icons> — the
  `timer`, `speed`, and `favorite` symbols
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

- Files: `crates/app/assets/icons/svg/{skull,mouse_off,pin,share,history}.svg`
  and the correspondingly named PNGs under `crates/app/assets/icons/glyphs/`
- Upstream: <https://github.com/Templarian/MaterialDesign> — the `skull`,
  `cursor-default-click-outline` (struck-through), `pin`, `export`, and
  `history` icons
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

## Monster, scene and skill name tables (community)

- Files: `crates/meter/data/MonsterName.json`, `crates/meter/data/SceneName.json`,
  `crates/meter/data/SkillName.json` (from `resonance-logs/resonance-logs`,
  GPL-3.0) and `crates/meter/data/MonsterNameCrowdsource.json` (from
  `winjwinj/bpsr-logs`, GPL-3.0).
- Upstream: <https://github.com/resonance-logs/resonance-logs>,
  <https://github.com/winjwinj/bpsr-logs>
- License: GPL-3.0, matching this project's own licence
- Not shipped as files: compiled into `crates/meter/src/tables.rs` by
  `scripts/gen-name-tables.py` at development time, then built into the
  executable as Rust source.
- For monsters and scenes, these are the hand-checked, player-facing names,
  and the generator ranks them *above* the BPSR-ZDPS tables below wherever
  the two disagree on an id. **`SkillName.json` is the exception (issue
  #16): it is an 8891-entry machine-translated bulk dump — it contains
  internal-only entries such as "AI: Air blade spike count" alongside real
  skill names — so for skills it is the *backfill* layer, ranked below
  BPSR-ZDPS's curated `SkillOverridesNames.json` in the section below.**

## Monster, scene and skill name tables (BPSR-ZDPS)

- Files: `crates/meter/data/MonsterTableNames.json`,
  `crates/meter/data/SceneTableNames.json`,
  `crates/meter/data/DungeonsTableNames.json`,
  `crates/meter/data/SkillOverridesNames.json`
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> —
  `BPSR-ZDPS/Data/MonsterTable.json`, `SceneTable.json`, `DungeonsTable.json`,
  `SkillOverrides.en.json`
- Copyright (c) 2025 Blue-Protocol-Source
- License: **MIT — note this differs from the GPL-3.0 of the community tables
  above.** MIT is the more permissive of the two and imposes only the
  attribution this section provides, so combining the two in a GPL-3.0-only
  work is fine; the combined result is governed by this project's GPL-3.0-only
  licence. BPSR-ZDPS states its licence once, at the repository root, with no
  carve-out for `Data/`.
- **Filtered, not verbatim.** Upstream `MonsterTable.json`, `SceneTable.json`
  and `DungeonsTable.json` are roughly 6.5 MB, 0.6 MB and 1.1 MB of full row
  data — stats, drop tables, AI references and asset paths. What is vendored
  here is only the `{id: name}` projection of each (about 97 KB, 19 KB and
  17 KB), produced by `filter_id_names` in `scripts/gen-name-tables.py`;
  nothing else from those rows is copied into this repository. `SkillOverrides.en.json`
  is filtered the same way, into two files: the `{id: Name}` projection
  (~56 KB) is vendored as `SkillOverridesNames.json`, and — since issue #192
  — the `{id: Icon}` projection (~11 KB, 327 of the 1487 rows carry a usable
  icon reference) as `SkillOverridesIcons.json`, produced by `filter_id_icons`
  in the same script. Since issue #247 the full client `SkillTable.json`
  (10.8 MB) is filtered by the same `filter_id_icons` into a third file, the
  `{id: Icon}` projection `SkillTableIcons.json` (~35 KB, 1,089 of its 4,796
  rows carry a usable icon reference); nothing else from those rows is copied.
  Only the last path segment of each `Icon` value is kept: upstream they are
  client atlas paths (`ui/atlas/skill_weapon_mz/weapon_mz-01_kx06`) and this
  project's asset directory is flat. Re-run
  `scripts/gen-name-tables.py --refresh` to reproduce the filtering from
  upstream.
- Added by issue #36 to backfill the ids the community tables do not cover:
  they take `monster_name` from 216 to 3,094 ids and `scene_name` from 340 to
  605, so far fewer encounters fall back to a raw `Monster #40010` /
  `Scene #1201` placeholder in the header. `SkillOverridesNames.json` was
  added by issue #16 as the curated, player-facing layer for `skill_name`
  (1487 entries), ranked above `SkillName.json`'s bulk backfill (see the
  community section above for why the precedence is inverted here).
- Not shipped as files: like the community tables, they are compiled into
  `crates/meter/src/tables.rs` at development time and built into the
  executable as Rust source.

## Boss-monster id list

- Files: `crates/meter/data/MonsterTableBossIds.json`
- Upstream: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> —
  `BPSR-ZDPS/Data/MonsterTable.json` (the same file `MonsterTableNames.json`
  above is filtered from, projected to ids instead of names)
- Copyright (c) 2025 Blue-Protocol-Source
- License: MIT, the same as the "Monster and scene name tables (BPSR-ZDPS)"
  section above and for the same reason: combining it into this GPL-3.0-only
  project is fine, and the combined result is governed by this project's
  GPL-3.0-only licence.
- **Filtered, not verbatim** (issue #112). `filter_boss_ids` in
  `scripts/gen-name-tables.py` projects `MonsterTable.json` down to the ids
  whose `MonsterType` field is 2 (`Zproto.EMonsterType::Boss`) — the same
  field and test the reference tool BPSR-ZDPS itself uses to classify an
  encounter (`Encounter.SetEntityType` -> `UpdateEncounterBossData`).
  `MonsterRank`, which the previous version of this list's generation logic
  relied on hand-curation instead of, is `""` for every shipped row and is not
  a trustworthy substitute.
- Compiled into the `BOSS_MONSTER_IDS` constant and `is_boss_monster` in
  `crates/meter/src/tables.rs` by `scripts/gen-name-tables.py`, the same as
  the tables above. Used to gate the top-bar encounter name to boss fights
  only (issue #42): `Meter::recompute_boss` has no boss/trash classification
  of its own, so this id set is consulted at display time to decide whether
  the resolved target is a real boss worth naming, rather than a large trash
  pull.
- A short manual-override list (`BOSS_ID_MANUAL_OVERRIDES` in
  `scripts/gen-name-tables.py`, not a vendored file) adds back one id —
  61,220, "Storm Goblin King" — that `MonsterType` does not mark as 2 but that
  the previous hand-curated list below carried as a boss and that is fought as
  a named miniboss; see the override's comment in the script for the full
  reasoning.
- Until issue #112 this list was hand-curated from
  `crates/meter/data/MonsterNameBoss.json` (GPL-3.0, from
  `winjwinj/bpsr-logs`, also shipped identically by
  `resonance-logs/resonance-logs`). That file has been removed from this
  repository; it is credited here only as the historical source of the one id
  preserved by `BOSS_ID_MANUAL_OVERRIDES` above.
