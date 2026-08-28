#!/usr/bin/env python3
"""Regenerate `crates/meter/src/tables.rs` from the vendored name tables.

Two families of source feed these tables, and they are merged, not chosen
between (issue #36):

*Curated community tables* (GPL-3.0, the same licence as this project) from the
`resonance-logs` and `bpsr-logs` trackers. Small, hand-checked, and generally
closer to what's on screen than a raw client dump — id 11019 is a bare "ID
Placeholder" in the authoritative table below, but the community files call
it "Boss - Darkened Python". Vendored verbatim under `crates/meter/data/`.
(A handful of ids run the other way — see `MONSTER_NAME_MANUAL_OVERRIDES`.)

*Authoritative game tables* (MIT) from `Blue-Protocol-Source/BPSR-ZDPS`'s
`Data/`. These are the full client tables the community files were derived
from, and they cover an order of magnitude more ids, so far fewer fall back to
a raw `Monster #40010` / `Scene #1201` placeholder in the header. The raw files
are multi-megabyte rows of stats, drop tables and asset paths; only the
`{id: name}` projection is vendored here (tens of KB), written by `--refresh`
below. See `THIRD_PARTY_NOTICES.md` — the licence differs from the community
files' GPL-3.0.

The curated name always wins where the two disagree; see `merge_names`.

Everything is compiled into `match` arms so the lookup is free at runtime and
the binary carries no JSON parser for them.

Usage:

    python3 scripts/gen-name-tables.py            # regenerate from vendored JSON
    python3 scripts/gen-name-tables.py --check    # offline: fail if tables.rs is stale
    python3 scripts/gen-name-tables.py --refresh  # re-download sources, then regenerate
    python3 scripts/gen-name-tables.py --refresh --check
                                                  # online: fail if any vendored copy
                                                  # has drifted from upstream

Run `cargo fmt -p bpsr-meter` after a regeneration. `.github/workflows/name-tables.yml`
runs both `--check` forms so neither the generated file nor the vendored copies
can rot silently.
"""

import argparse
import functools
import io
import json
import pathlib
import re
import sys
import unicodedata
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
DATA = ROOT / "crates" / "meter" / "data"
OUT = ROOT / "crates" / "meter" / "src" / "tables.rs"

_RESONANCE = "https://raw.githubusercontent.com/resonance-logs/resonance-logs/main/src-tauri/meter-data"
_BPSR_LOGS = "https://raw.githubusercontent.com/winjwinj/bpsr-logs/main/src/lib/data/json"
_ZDPS = "https://raw.githubusercontent.com/Blue-Protocol-Source/BPSR-ZDPS/master/BPSR-ZDPS/Data"

# Curated community tables, vendored byte-for-byte: they are already tiny
# `{id: name}` maps, and keeping them verbatim keeps their provenance checkable.
VERBATIM_SOURCES = {
    "MonsterName.json": f"{_RESONANCE}/MonsterName.json",
    "SceneName.json": f"{_RESONANCE}/SceneName.json",
    "MonsterNameCrowdsource.json": f"{_BPSR_LOGS}/MonsterNameCrowdsource.json",
    # Issue #16: resonance-logs' machine-translated bulk dump — 8891 ids
    # including internal-only entries ("AI: Air blade spike count"). This is
    # the *backfill* layer for skills, the inverse of the monster/scene tables
    # above where the community file is the hand-checked one — see
    # `SkillOverridesNames.json` below and the merge order in `render()`.
    "SkillName.json": f"{_RESONANCE}/SkillName.json",
}

# Authoritative game tables, vendored *filtered*. Upstream these are 6.5 MB,
# 0.6 MB and 1.1 MB of full row data; `filter_id_names` reduces each to the
# `{id: name}` pairs that are all this project wants from them.
FILTERED_SOURCES = {
    "MonsterTableNames.json": f"{_ZDPS}/MonsterTable.json",
    "SceneTableNames.json": f"{_ZDPS}/SceneTable.json",
    "DungeonsTableNames.json": f"{_ZDPS}/DungeonsTable.json",
    # Issue #16: BPSR-ZDPS's curated player-facing skill names — 1487
    # hand-tuned entries, `{id: {Name, Icon}}`. Unlike the monster/scene tables
    # above, *this* is the hand-checked layer for skills, not `SkillName.json`
    # — see the merge order in `render()`. `filter_id_names` already handles
    # this shape unchanged; the `Icon` half of each row is projected out
    # separately by `ICON_SOURCES` below.
    "SkillOverridesNames.json": f"{_ZDPS}/SkillOverrides.en.json",
}

# Issue #192: the same `SkillOverrides.en.json` rows also carry `Icon`, an
# asset path into the game client's UI atlas
# (`ui/atlas/skill_weapon_mz/weapon_mz-01_kx06`). This used to be dropped on
# the floor because there were no skill-icon assets to key against it; the
# skill breakdown window now paints one per row, so the id -> icon *basename*
# half is vendored here, projected from the same URL `SkillOverridesNames.json`
# uses (`_fetch` is `lru_cache`d, so a `--refresh` still downloads it once).
#
# Only the basename is kept: the atlas directories are a client-side layout
# this project does not mirror, and `crates/app/assets/skills/` is flat. The
# PNGs themselves are vendored by `scripts/prep-skill-icons.py` and compiled in
# via `scripts/gen-skill-icons.py`; an id whose basename has no committed PNG
# degrades to a blank placeholder at draw time, so this table is deliberately
# allowed to name icons that are not shipped.
#
# Issue #247: `SkillOverrides.en.json` is *curated*, and only 328 of its 1487
# rows carry an `Icon` at all — so keying the row list off it alone left the
# majority of skill ids seen in real play with no icon and a blank placeholder.
# The full client `SkillTable.json` carries the same `Icon` field for 1089 of
# its 4796 rows (441 distinct basenames), and is the same authoritative-table
# backfill relationship `MonsterTableNames.json` already has with the community
# monster files. It is merged *under* the overrides in `render()`, so a curated
# icon still wins wherever both name one. Neither table names an icon for every
# id: an id both leave iconless (a proc/DoT damage source such as 2031103
# "Lucky Strike (Battle Axe)", which lives in `BuffTable.json` with `Icon: ""`)
# still degrades to the blank placeholder — there is nothing upstream to draw.
ICON_SOURCES = {
    "SkillOverridesIcons.json": f"{_ZDPS}/SkillOverrides.en.json",
    "SkillTableIcons.json": f"{_ZDPS}/SkillTable.json",
}

# Issue #112: the same `MonsterTable.json` also carries `MonsterType`, an enum
# (`Zproto.EMonsterType`: Monster = 0, Elite = 1, Boss = 2) that the reference
# tool BPSR-ZDPS itself uses to classify encounters (`Encounter.SetEntityType`
# -> `UpdateEncounterBossData`, tested against `MonsterType == 2`). That is a
# trustworthy, fully-populated boss flag — unlike the tempting-looking
# `MonsterRank` field, which is `""` for every one of the table's 3094 rows in
# the shipped data (a dead, unshipped field) and the attrs
# `AttrIsMonsterRankEnable` (459) / `AttrMonsterRank` (460), which the
# reference tool's IL never reads and no enum gives meaning to. Do not resurrect
# either. This is filtered from the same URL as `MonsterTableNames.json` above,
# just projected to ids instead of names.
BOSS_ID_SOURCES = {
    "MonsterTableBossIds.json": f"{_ZDPS}/MonsterTable.json",
}

# Issue #125: `DungeonsTable.json`'s `SceneID` column lists the scene id each
# dungeon instance loads into — 572 distinct ids (min 1001, max 171001; 571 of
# 572 also appear in `SceneTable.json`, the exception being 20043). This is
# what `is_dungeon_scene` uses to gate the final-boss latch in
# `Meter::recompute_boss` to actual dungeons: without it, killing a world
# boss in an open-world zone would pin its name to the banner for every later
# visit to that town or field. This is filtered from the same URL as
# `DungeonsTableNames.json` above, just projected to scene ids instead of
# names.
DUNGEON_SCENE_ID_SOURCES = {
    "DungeonSceneIds.json": f"{_ZDPS}/DungeonsTable.json",
}

# Issue #201: the curated dungeon scene -> final-boss table. Deliberately *not*
# in any of the source dicts above — it is hand-maintained here, never
# downloaded, so `--refresh` must leave it alone. Issue #125's investigation
# established that nothing upstream maps a dungeon to its boss (every
# `BPSR-ZDPS/Data/*.json` table and both community repos were checked), which
# is why issue #131 learned it at runtime instead. Issue #201 retired that
# machinery — there are few enough dungeons to simply write them down — so this
# file is now the only source, and it is filled in by hand from real runs.
#
# `filter_scene_final_bosses` validates every row against the generated name
# tables, so a typo or a game patch that re-ids a boss fails the `generated` CI
# job instead of silently shipping a wrong header caption.
SCENE_FINAL_BOSSES_FILE = "SceneFinalBosses.json"

# Manual overrides (issue #112): ids the old hand-curated `MonsterNameBoss.json`
# list carried as bosses that `MonsterTable.json`'s `MonsterType` does not mark
# as `Boss` (2). Checked individually — see the per-id comment — and unioned on
# top of the generated `MonsterType == 2` set; this can only ever add ids back,
# never suppress one the generated set already includes.
BOSS_ID_MANUAL_OVERRIDES: dict[int, str] = {
    # "Storm Goblin King": MonsterType 1 (Elite) in MonsterTable.json, not 2.
    # Both community trackers (`resonance-logs` and `bpsr-logs`) flagged it as
    # a boss in their curated files, and it is fought as a named miniboss with
    # its own encounter, so it is kept rather than silently losing its header
    # display when the source of truth switched to MonsterType.
    61_220: "Storm Goblin King",
    # The World Dominator daily rotation (issue #313). Scene 7152 cycles a
    # different world boss each night; two consecutive sessions
    # (2026-08-25 23:37 and 2026-08-26 23:21) ended their pull against
    # 3000022 and 3000063 respectively, both unrecognized, both wiping the
    # meter mid-pull with `reset reason=NewFight` at 35.7% / 41.8% boss HP.
    #
    # `MonsterTable.json` marks both `MonsterType == 0`, but that block's
    # classification is stale rather than considered: 3000000..=3000081 is
    # the World Dominator registry — 81 rows, of which only 5 carry
    # `MonsterType == 2` (those 5 are already in the generated set) and the
    # remaining 76 carry `MonsterType == 0` *and* `BloodTubeCount == 0`. A
    # world boss with no health bars at all is not a plain monster; the field
    # was simply never filled in for these rows. 3000063's own base template,
    # 1701 "Denvel", *is* `MonsterType == 2` and is already in the set, which
    # is the tell.
    3_000_063: "Denvel",
    3_000_022: "Muku Chief - Resonance",
}

# Manual overrides (issue #313): dungeon scene ids `DungeonsTable.json`'s
# `SceneID` column omits but that behave as instances in the logs. Same idiom
# and same one-way guarantee as `BOSS_ID_MANUAL_OVERRIDES` above — unioned on
# top of the generated set, so this can only ever add ids, never suppress one.
DUNGEON_SCENE_ID_MANUAL_OVERRIDES: dict[int, str] = {
    # "World Dominator": `SceneTableNames.json` names 7150, 7151 and 7152
    # identically, and 7150/7151 are both already in the generated set —
    # upstream's `DungeonsTable.SceneID` simply has no row pointing at 7152.
    # The 2026-08-26 log shows the client tearing down and re-establishing its
    # server connection to enter it, and `cause=dungeon_ended` firing on the
    # way out: it is an instance by every behaviour the meter can observe.
    # Without it here, issue #151's in-dungeon fight hold
    # (`Meter::engaged_boss_still_up`) never engages in a world-boss scene and
    # a 9s immunity window reads as the end of the encounter.
    7_152: "World Dominator",
}

# Manual overrides (issue #313): monster names the *curated* community layer
# gets wrong — the last word on ids where the shipped client's own name is the
# one players see. This is the only layer above `MonsterName.json`, and it
# exists because the curated-wins precedence (see `merge_names` and `HEADER`)
# is right in general but not universally: the community trackers carry a
# pre-release name for some templates that the live client no longer uses.
#
# Deliberately per-id rather than a global precedence flip: `MonsterName.json`
# and `MonsterTableNames.json` disagree on 59 ids, and the curated name is the
# better one on nearly all of them.
MONSTER_NAME_MANUAL_OVERRIDES: dict[int, str] = {
    # The Ignisor family. `MonsterName.json` calls all four "Rathalos"; every
    # one of them is "Ignisor" in `MonsterTableNames.json`, which is the name
    # the shipped client renders and the name the issue #313 reporter read off
    # their own screen for 20004. (Id 104 is the same monster and already
    # resolves to "Ignisor" — no community file names it, so it needs no
    # override.)
    103: "Ignisor",
    1_013: "Ignisor",
    20_004: "Ignisor",
    60_021: "Ignisor",
}

HEADER = """//! Static id -> display-name tables for monsters, scenes and skills.
//!
//! Generated by `scripts/gen-name-tables.py`. Do not edit by hand.
//!
//! Merged from two families of source under `crates/meter/data/` (issue #36;
//! skills added by issue #16): the curated community tables shipped by the
//! `resonance-logs` and `bpsr-logs` trackers (GPL-3.0, the same licence as
//! this project), and the authoritative game tables from
//! `Blue-Protocol-Source/BPSR-ZDPS` (MIT), filtered down to their
//! `{id: name}` pairs. See `THIRD_PARTY_NOTICES.md`.
//!
//! Where two sources disagree, the hand-checked layer wins. For monsters and
//! scenes that is the community table, over the client's raw internal name.
//! For skills it is the other way round: BPSR-ZDPS's `SkillOverrides.en.json`
//! is a 1487-entry curated, player-facing list, while resonance-logs'
//! `SkillName.json` is an 8891-entry machine-translated bulk dump (it
//! contains internal-only entries such as "AI: Air blade spike count"), so
//! *it* is the backfill layer here. The precedence rule is unchanged
//! (hand-checked wins); only which file is hand-checked differs per table.
//!
//! Coverage is broad but not total - an id absent from both families yields
//! `None`, and callers fall back to showing the raw id.
"""

FOOTER = """
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_names_win_over_the_authoritative_table() {
        // BPSR-ZDPS's `MonsterTable.json` carries a bare "ID Placeholder" for
        // template 11019; the community tables name it "Boss - Darkened
        // Python". The curated community name has to survive the backfill —
        // this is the precedence in `merge_names` observed end to end.
        //
        // (This test used to point at 1013, where the client says "Ignisor"
        // and the community tables say "Rathalos". Issue #313 established
        // that "Ignisor" is what the live client actually renders, so that id
        // is now pinned the other way by `MONSTER_NAME_MANUAL_OVERRIDES` —
        // see `the_ignisor_ids_keep_the_clients_own_name`. A placeholder-vs-
        // real-name pair illustrates why the curated layer wins far better
        // than a name-vs-name pair the client turned out to win anyway.)
        assert_eq!(monster_name(11_019), Some("Boss - Darkened Python"));
    }

    #[test]
    fn the_ignisor_ids_keep_the_clients_own_name() {
        // Issue #313: `MonsterName.json` calls this family "Rathalos", but
        // the shipped client renders "Ignisor" — the reporter read it off
        // their own screen for 20004, mid-pull, while the header showed
        // "Rathalos". `MONSTER_NAME_MANUAL_OVERRIDES` is layered above the
        // curated table for exactly these ids, and nothing else.
        assert_eq!(monster_name(20_004), Some("Ignisor"));
        assert_eq!(monster_name(103), Some("Ignisor"));
        assert_eq!(monster_name(1_013), Some("Ignisor"));
        assert_eq!(monster_name(60_021), Some("Ignisor"));
        // 104 is the same monster but absent from every community file, so it
        // was already correct via the authoritative backfill. Pinned so a
        // future precedence change cannot quietly rename it out from under
        // the four above.
        assert_eq!(monster_name(104), Some("Ignisor"));
    }

    #[test]
    fn authoritative_backfill_names_ids_no_community_file_covers() {
        // Template 205 appears only in BPSR-ZDPS's `MonsterTable.json`; no
        // community file names it, so before the backfill this id fell back
        // to a raw `Monster #205` placeholder in the header (issue #36).
        assert_eq!(monster_name(205), Some("Void Bzzar"));
    }

    #[test]
    fn a_backfilled_monster_is_not_promoted_to_boss() {
        // The backfill multiplies named monster ids more than tenfold, so far
        // more trash now carries a name `recompute_boss` could hoist into the
        // header. Having a name must never imply bosshood: the display gate is
        // `is_boss_monster`, which reads only ids `MonsterTable.json` marks
        // `MonsterType == 2` (issue #112) and is deliberately *not* widened by
        // the backfill.
        assert!(monster_name(205).is_some());
        assert!(!is_boss_monster(205));
    }

    #[test]
    fn authoritative_backfill_names_open_world_scenes() {
        // `SceneName.json` catalogues instances; the open-world zone ids only
        // arrive with `SceneTable.json`.
        assert_eq!(scene_name(8), Some("Asterleeds"));
    }

    #[test]
    fn is_boss_monster_true_for_a_known_boss_id() {
        assert!(is_boss_monster(103));
    }

    #[test]
    fn is_boss_monster_false_for_a_known_non_boss_monster_id() {
        // "Golden Nappo" (10900) has a name in `monster_name` but its
        // `MonsterTable.json` `MonsterType` is 0 (plain Monster), not 2 — a
        // named-but-non-boss id must not read as a boss just because the
        // tables happen to know its name.
        assert!(!is_boss_monster(10_900));
    }

    #[test]
    fn is_boss_monster_true_for_the_issue_112_boss_ids() {
        // These template ids jumped straight from 102721 to 130110 in the
        // old hand-curated list — no 103xxx id at all — so real
        // current-content bosses fell through to a blank header mid-fight.
        // All four have `MonsterType == 2` in `MonsterTable.json`.
        assert!(is_boss_monster(103_108)); // Paradox-Calamity Remnant - Origin
        assert!(is_boss_monster(103_111)); // Dragonbane Golem - Left Cannon
        assert!(is_boss_monster(103_207)); // Paradox-Calamity Remnant - Continuation
        assert!(is_boss_monster(103_308)); // Paradox-Calamity Remnant - Final
    }

    #[test]
    fn is_boss_monster_true_for_a_manual_override_not_marked_boss_by_monster_type() {
        // "Storm Goblin King": MonsterType 1 (Elite) in MonsterTable.json,
        // not 2, but both community trackers flagged it as a boss and it is
        // fought as one — see `BOSS_ID_MANUAL_OVERRIDES` in
        // `scripts/gen-name-tables.py`.
        assert!(is_boss_monster(61_220));
    }

    #[test]
    fn is_boss_monster_true_for_the_world_dominator_rotation() {
        // Issue #313: scene 7152 rotates its world boss nightly, and two
        // consecutive sessions ended on these two ids. `MonsterTable.json`
        // marks both `MonsterType == 0`, but so is every one of the 76
        // non-boss rows in the 3000000..=3000081 World Dominator registry,
        // and all 76 also carry `BloodTubeCount == 0` — a world boss with no
        // health bars is an unfilled row, not a classification. Unrecognized,
        // they cost the meter a full wipe mid-pull: `is_engaged_recognized_boss`
        // went false, issue #151's fight hold dropped, and a 9s immunity
        // window ended the encounter at 41.8% boss HP.
        assert!(is_boss_monster(3_000_063)); // Denvel; base template 1701 is MonsterType 2
        assert!(is_boss_monster(3_000_022)); // Muku Chief - Resonance
    }

    #[test]
    fn is_dungeon_scene_true_for_every_world_dominator_scene() {
        // Issue #313: all three of these are named "World Dominator" in
        // `SceneTableNames.json`, but upstream's `DungeonsTable.SceneID`
        // lists only the first two, so 7152 alone fell out of the instance
        // set — and with it out of `Meter::engaged_boss_still_up`'s
        // `in_dungeon_scene()` guard. 7152 is restored by
        // `DUNGEON_SCENE_ID_MANUAL_OVERRIDES`.
        assert!(is_dungeon_scene(7150));
        assert!(is_dungeon_scene(7151));
        assert!(is_dungeon_scene(7152));
        assert_eq!(scene_name(7152), Some("World Dominator"));
    }

    #[test]
    fn is_boss_monster_boundary_ids() {
        assert!(is_boss_monster(103)); // lowest id in the set
        assert!(is_boss_monster(7_700_001)); // highest id in the set
        assert!(!is_boss_monster(102)); // just below the lowest
        assert!(!is_boss_monster(7_700_002)); // just above the highest
    }

    #[test]
    fn is_dungeon_scene_true_for_known_dungeon_scene_ids() {
        // issue #125: sample dungeon scene ids from `DungeonsTable.json`'s
        // `SceneID` column, spanning early- and late-game content.
        assert!(is_dungeon_scene(1001)); // Tina's Mindrealm
        assert!(is_dungeon_scene(1101)); // Towering Ruin
        assert!(is_dungeon_scene(1201)); // Dragon Claw Valley
        assert!(is_dungeon_scene(7050)); // Goblin Rampage
    }

    #[test]
    fn is_dungeon_scene_false_for_an_open_world_scene() {
        // Scene 8 ("Asterleeds") is an open-world zone, not a dungeon
        // instance: it must never latch a remembered boss (issue #125),
        // which is exactly what `is_dungeon_scene` gates.
        assert!(!is_dungeon_scene(8));
    }

    #[test]
    fn crowdsourced_entries_are_present_in_the_generated_table() {
        // NB: this only proves `MonsterNameCrowdsource.json` entries make it
        // into the table at all — it shares every overlapping id with
        // `MonsterName.json` at a byte-identical value, so it cannot show
        // which of those two wins. Their relative precedence is covered by
        // `merge_names`'s self-test in `scripts/gen-name-tables.py`; the
        // curated-over-authoritative precedence, which *is* observable here,
        // is covered by `curated_names_win_over_the_authoritative_table`.
        assert_eq!(monster_name(10086), Some("Goblin King"));
    }

    #[test]
    fn names_a_known_scene() {
        assert_eq!(scene_name(1001), Some("Tina's Mindrealm"));
    }

    #[test]
    fn unknown_ids_are_none() {
        assert_eq!(monster_name(0), None);
        assert_eq!(scene_name(0), None);
    }

    #[test]
    fn curated_skill_overrides_win_over_the_bulk_backfill() {
        // Issue #16: skills invert the monster/scene precedence — here
        // `SkillOverridesNames.json` (BPSR-ZDPS, 1487 hand-tuned entries) is
        // the curated layer and `SkillName.json` (resonance-logs, 8891
        // machine-translated entries, including internal-only rows like
        // "AI: Air blade spike count") is the bulk backfill. Id 1602: the
        // backfill calls it "Basic Attack: Scorching Swing"; the curated
        // override calls it "Blazing Swing - Stage 2", which must win.
        assert_eq!(skill_name(1602), Some("Blazing Swing - Stage 2"));
    }

    #[test]
    fn scene_final_boss_names_a_curated_single_boss_dungeon() {
        // Issue #201: scene 1154 ("Unstable - Towering Ruin") -> monster 1152
        // ("Kartgriff"), from `crates/meter/data/SceneFinalBosses.json`.
        assert_eq!(scene_final_boss(1154), Some(1152));
    }

    #[test]
    fn scene_final_boss_is_none_for_an_uncurated_dungeon_scene() {
        // Most dungeons are not curated, and that is fine — the header simply
        // has no caption for them until a boss is engaged.
        assert!(is_dungeon_scene(1001));
        assert_eq!(scene_final_boss(1001), None);
    }

    #[test]
    fn scene_final_boss_is_none_for_a_non_dungeon_scene() {
        assert_eq!(scene_final_boss(8), None);
    }

    #[test]
    fn every_curated_scene_final_boss_resolves_in_the_generated_tables() {
        // The Rust-side half of `filter_scene_final_bosses`' validation: a
        // curated entry must name a real dungeon scene and a real, *named*
        // boss monster, since an unnamed one would caption nothing at all.
        for &(scene, boss) in SCENE_FINAL_BOSSES {
            assert!(is_dungeon_scene(scene), "scene {scene} is not a dungeon");
            assert!(is_boss_monster(boss), "monster {boss} is not a boss");
            assert!(scene_name(scene).is_some(), "scene {scene} is unnamed");
            assert!(monster_name(boss).is_some(), "monster {boss} is unnamed");
        }
    }

    #[test]
    fn curated_scene_final_bosses_are_sorted_and_unique() {
        // `scene_final_boss` binary-searches, so unsorted or duplicated scene
        // ids would silently stop resolving.
        let scenes: Vec<u32> = SCENE_FINAL_BOSSES.iter().map(|&(scene, _)| scene).collect();
        let mut sorted = scenes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(scenes, sorted);
    }

    #[test]
    fn unknown_skill_ids_are_none() {
        assert_eq!(skill_name(0), None);
    }
}
"""


def esc(s: str) -> str:
    """Escape `s` for embedding in a Rust `&'static str` literal.

    Order matters: backslash is escaped first, so the escapes this function
    inserts below are never themselves re-escaped. `\\r`, `\\n`, and `\\t`
    get Rust's short forms; every other control character gets Rust's
    `\\u{...}` form, so a bare CR (or any other control byte) in a future
    refresh of the vendored JSON can never produce a `tables.rs` that fails
    to compile with "bare CR not allowed in string" or similar.
    """
    s = s.replace("\\", "\\\\").replace('"', '\\"')
    out = []
    for ch in s:
        if ch == "\r":
            out.append("\\r")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 0x20 or ord(ch) == 0x7F or _is_invisible(ch):
            out.append(f"\\u{{{ord(ch):x}}}")
        else:
            out.append(ch)
    return "".join(out)


def _is_invisible(ch: str) -> bool:
    """Whether `ch` must be escaped because it is invisible in source.

    Clippy's `invisible_characters` lint rejects zero-width and unusual space
    characters written literally in a string, and CI compiles with
    `-D warnings` — so a name carrying one would fail the build rather than
    merely look odd. The client tables really do carry them: three names have
    a stray U+200B. Escaping preserves the string exactly while keeping the
    generated source lintable.

    Categories rather than a hard-coded list, so a future refresh introducing
    a different invisible character is handled too. `Cf` is format characters
    (zero-width space, joiners, bidi marks); `Zs` is spaces other than plain
    ASCII space, which is left alone. Visible non-ASCII — the middle dot in
    "Rin·Izcorgiky", the "×" in "Serpent Egg ×1" — is not touched.
    """
    return ch != " " and unicodedata.category(ch) in ("Cf", "Zs")


def filter_id_names(raw: dict) -> dict[str, str]:
    """Project a full BPSR-ZDPS `Data/*Table.json` down to `{id: name}`.

    Upstream each row is a large object of stats, asset paths and references;
    all this project wants is `Name`. Rows whose `Name` is missing, non-string
    or blank are dropped rather than vendored as empty strings, so an unnamed
    id keeps falling back to its raw-id placeholder instead of rendering as an
    empty header.
    """
    out = {}
    for key, row in raw.items():
        if not isinstance(row, dict):
            continue
        name = row.get("Name")
        if isinstance(name, str) and name.strip():
            out[str(int(key))] = name.strip()
    return dict(sorted(out.items(), key=lambda kv: int(kv[0])))


# An icon basename we are willing to treat as an asset name: what a file under
# `crates/app/assets/skills/` can actually be called. Upstream's `Icon` field is
# not uniformly an asset path — at least one row carries free prose ("From
# Shield Combo talent") — and a value with spaces in it can never name a
# committed PNG, so it is dropped here rather than vendored as a permanently
# unresolvable key.
_ICON_BASENAME_RE = re.compile(r"^[A-Za-z0-9_.-]+$")


def filter_id_icons(raw: dict) -> dict[str, str]:
    """Project a BPSR-ZDPS skill table down to `{id: icon basename}`.

    The inverse projection of `filter_id_names` over the same rows (issue
    #192), applied to both `SkillOverrides.en.json` and, since issue #247, the
    full `SkillTable.json`. Upstream `Icon` is a client atlas path
    (`ui/atlas/skill_weapon_mz/weapon_mz-01_kx06`); only the last path segment
    is kept, since that is what the vendored PNGs are named. Rows whose `Icon`
    is missing, non-string, blank, or not a plausible asset basename are
    dropped, so an iconless id keeps falling through to the draw-time blank
    placeholder rather than being vendored as an empty string.
    """
    out = {}
    for key, row in raw.items():
        if not isinstance(row, dict):
            continue
        icon = row.get("Icon")
        if not isinstance(icon, str):
            continue
        basename = icon.strip().rsplit("/", 1)[-1]
        if _ICON_BASENAME_RE.match(basename):
            out[str(int(key))] = basename
    return dict(sorted(out.items(), key=lambda kv: int(kv[0])))


def filter_boss_ids(raw: dict) -> list[int]:
    """Project a full `MonsterTable.json` down to ids with `MonsterType == 2`
    (`Zproto.EMonsterType.Boss`) — see `BOSS_ID_SOURCES` for why this field,
    not `MonsterRank`, is the trustworthy one.
    """
    out = []
    for key, row in raw.items():
        if isinstance(row, dict) and row.get("MonsterType") == 2:
            out.append(int(key))
    return sorted(out)


def filter_dungeon_scene_ids(raw: dict) -> list[int]:
    """Project a full `DungeonsTable.json` down to distinct dungeon scene ids
    (issue #125) — the `SceneID` each row's dungeon instance loads into. See
    `DUNGEON_SCENE_ID_SOURCES` for why this gates the final-boss latch.
    """
    out = []
    for row in raw.values():
        if isinstance(row, dict):
            scene_id = row.get("SceneID")
            if scene_id:
                out.append(int(scene_id))
    return sorted(set(out))


def filter_scene_final_bosses(
    raw: dict,
    scenes: dict[int, str],
    monsters: dict[int, str],
    boss_ids: set[int],
    dungeon_scene_ids: set[int],
) -> list[tuple[int, int]]:
    """Validate and flatten `SceneFinalBosses.json` into sorted `(scene, boss)`
    pairs (issue #201).

    Every row is checked against the tables generated alongside it: the scene
    has to be a real dungeon scene, the boss a real boss monster, and both
    `scene`/`boss` name fields have to still match what `scene_name` /
    `monster_name` resolve to. All problems are collected and reported at once
    rather than failing on the first, so a refresh that moved several ids
    reads as one list of edits to make. Keys starting with `_` are prose
    (`_README`) and are skipped.
    """
    problems: list[str] = []
    pairs: list[tuple[int, int]] = []
    for key, row in raw.items():
        if key.startswith("_"):
            continue
        scene = int(key)
        boss = int(row["boss_id"])
        if scene not in dungeon_scene_ids:
            problems.append(f"scene {scene} is not in DungeonSceneIds.json")
        if boss not in boss_ids:
            problems.append(f"monster {boss} (scene {scene}) is not a boss monster")
        for field, table, want_id in (("scene", scenes, scene), ("boss", monsters, boss)):
            got = table.get(want_id)
            if got != row[field]:
                problems.append(
                    f"scene {scene}: {field!r} is {row[field]!r} but the generated "
                    f"table says {got!r} for id {want_id}"
                )
        pairs.append((scene, boss))

    if problems:
        listing = "\n  ".join(problems)
        sys.exit(f"{SCENE_FINAL_BOSSES_FILE} is out of date:\n  {listing}")

    pairs.sort()
    return pairs


def merge_names(*layers: dict[int, str]) -> dict[int, str]:
    """Merge id -> name layers, least authoritative first.

    Later layers overwrite earlier ones, so callers pass the bulk
    authoritative client table first and the hand-checked community tables
    last. Pulled out as its own function so `_self_test` can exercise the
    precedence directly: whether a merge is correct or silently reversed is
    not something the generated `tables.rs` can be read to decide.
    """
    merged: dict[int, str] = {}
    for layer in layers:
        merged.update(layer)
    return merged


def _keyed(raw: dict) -> dict[int, str]:
    """Reindex a `{id: name}` JSON map by integer id."""
    return {int(k): v for k, v in raw.items()}


def _self_test() -> None:
    """Fast synthetic checks run on every generation. There is no Python
    test harness elsewhere in this repo (no pytest config, no CI job that
    runs Python beyond this script — see `scripts/` and `.github/workflows/`),
    so this hard-fails inline instead of living in an untested, never-run
    test file.
    """
    merged = merge_names({1: "Client", 2: "Client Name"}, {2: "Curated Name"})
    assert merged == {1: "Client", 2: "Curated Name"}, (
        "later layers must win: curated names override the client table"
    )
    assert merge_names({1: "a"}, {}, {1: "b"}) == {1: "b"}
    assert merge_names() == {}

    filtered = filter_id_names(
        {
            "10": {"Name": "Kept", "Junk": [1, 2, 3]},
            "11": {"Name": "   "},
            "12": {"Name": None},
            "13": {"NoName": 1},
            "14": "not a row",
            "9": {"Name": "Sorted First"},
            "15": {"Name": "  Padded  "},
        }
    )
    assert filtered == {
        "9": "Sorted First",
        "10": "Kept",
        "15": "Padded",
    }, filtered
    assert list(filtered) == ["9", "10", "15"], "filtered output must be id-sorted"
    assert filtered["15"] == "Padded", (
        "a non-blank name carrying leading/trailing padding must be stored stripped"
    )

    icons = filter_id_icons(
        {
            "20": {"Icon": "ui/atlas/skill_weapon_mz/weapon_mz-01_kx06"},
            "10": {"Icon": "flat_basename"},
            "11": {"Icon": "  ui/atlas/x/padded  "},
            "12": {"Icon": "From Shield Combo talent"},
            "13": {"Icon": ""},
            "14": {"Icon": None},
            "15": {"NoIcon": 1},
            "16": "not a row",
        }
    )
    assert icons == {
        "10": "flat_basename",
        "11": "padded",
        "20": "weapon_mz-01_kx06",
    }, icons
    assert list(icons) == ["10", "11", "20"], "icon output must be id-sorted"

    boss_ids = filter_boss_ids(
        {
            "20": {"MonsterType": 2},
            "10": {"MonsterType": 0},
            "15": {"MonsterType": 2},
            "11": {"MonsterType": 1},
            "12": {"NoType": 1},
            "13": "not a row",
        }
    )
    assert boss_ids == [15, 20], boss_ids

    dungeon_scene_ids = filter_dungeon_scene_ids(
        {
            "1": {"SceneID": 1001},
            "2": {"SceneID": 0},
            "3": {"SceneID": 1001},
            "4": {"SceneID": None},
            "5": {"NoSceneID": 1},
            "6": "not a row",
            "7": {"SceneID": 1201},
        }
    )
    assert dungeon_scene_ids == [1001, 1201], dungeon_scene_ids

    assert esc('a\\b"c') == 'a\\\\b\\"c'
    assert esc("line1\rline2") == "line1\\rline2"
    assert esc("line1\nline2") == "line1\\nline2"
    assert esc("a\tb") == "a\\tb"
    assert esc("\x01") == "\\u{1}"
    assert esc("\x7f") == "\\u{7f}"
    # Invisible characters must be escaped, not emitted literally: clippy's
    # `invisible_characters` lint fires on the source character, and CI builds
    # with `-D warnings`. Three names in the client tables carry a stray
    # U+200B ("​Shadow Warden", "Loom of Dreams​"), so this is a real
    # refresh hazard, not a hypothetical one.
    assert esc("​Shadow Warden") == "\\u{200b}Shadow Warden"
    assert esc(" ") == "\\u{a0}"  # no-break space
    assert esc("　") == "\\u{3000}"  # ideographic space
    assert esc("﻿") == "\\u{feff}"  # zero-width no-break space
    # Visible non-ASCII is left alone — these are real characters in real
    # names ("Rin·Izcorgiky", "Serpent Egg ×1", "Light–Dark Entwined Entity").
    assert esc("Rin·Izcorgiky") == "Rin·Izcorgiky"
    assert esc("×–") == "×–"
    assert esc(" ") == " "  # a plain ASCII space is not an invisible character
    # Order: backslash-escaping must not re-escape a literal `\r` that was
    # already produced for a real carriage return.
    assert esc("\\\r") == "\\\\\\r"

    # BOM handling must be uniform across the write path (`_render_source`)
    # and the read path (`render()`'s loader), so neither can crash on a
    # BOM'd vendored file. `utf-8-sig` mirrors what both use.
    bom = b"\xef\xbb\xbf"
    assert json.loads((bom + b'{"a": 1}').decode("utf-8-sig")) == {"a": 1}
    assert (bom + b'{"a": 1}').removeprefix(bom) == b'{"a": 1}'


def emit(out: io.StringIO, doc: str, fn: str, table: dict) -> None:
    # Issue #16: `SkillName.json` contains machine-translated entries well
    # past rustfmt's line-length limit, which rustfmt would otherwise wrap
    # onto their own line/block — a rewrite `--check` (which compares this
    # raw, unformatted render against the committed, `cargo fmt`-passed file)
    # can never agree with. `#[rustfmt::skip]` keeps this match block exactly
    # as rendered, so the two checks can't fight each other; it is a no-op
    # for `monster_name`/`scene_name`, whose entries were always short enough
    # that rustfmt never touched them.
    out.write(f"\n{doc}\n#[rustfmt::skip]\npub fn {fn}(id: u32) -> Option<&'static str> {{\n")
    out.write("    Some(match id {\n")
    for key in sorted(table):
        out.write('        %d => "%s",\n' % (key, esc(table[key])))
    out.write("        _ => return None,\n    })\n}\n")


SKILL_ICON_DOC = """/// Icon basename for a skill id (issue #192), if BPSR-ZDPS names one for it.
/// The basename keys `crates/app/assets/skills/<basename>.png`, but this table
/// is deliberately wider than that directory: it names every icon upstream
/// references, and `crates/app/src/skill_icons.rs` only compiles in the ones
/// whose art is actually shipped upstream and therefore committed here. A
/// basename with no committed PNG resolves to `None` at the texture lookup and
/// the row paints a blank placeholder — never a panic.
///
/// Generated from two vendored layers, same precedence as `skill_name`\'s
/// (issue #247): `crates/meter/data/SkillTableIcons.json`, the `Icon` column of
/// the full client `SkillTable.json`, backfills
/// `crates/meter/data/SkillOverridesIcons.json`, the `Icon` half of the same
/// curated rows `skill_name`\'s hand-checked layer comes from. Neither names an
/// icon for every id — a proc/DoT damage source that lives only in
/// `BuffTable.json` with a blank `Icon` yields `None` here."""

BOSS_IDS_DOC = """/// Boss-monster template ids (issue #42): the top-bar encounter name should
/// only ever appear for a genuine boss fight. `Meter::recompute_boss`
/// (`crates/meter/src/encounter.rs`) picks whichever damaged enemy has the
/// largest known `max_hp` — a pure heuristic with no boss/trash
/// classification — so without this list a big trash mob would flash its
/// name in the header exactly like a real boss. This gates *display* only:
/// `EncounterInfo::boss_monster_id` stays populated for every pull regardless
/// of membership here; only `boss_name`/`is_boss` (set in `Meter::snapshot`)
/// are gated by it.
///
/// Generated (issue #112) from `crates/meter/data/MonsterTableNames.json`'s
/// source, BPSR-ZDPS's `MonsterTable.json`, as every id whose `MonsterType`
/// is 2 (`Zproto.EMonsterType::Boss`) — the same field and the same test the
/// reference tool BPSR-ZDPS itself uses to classify an encounter
/// (`Encounter.SetEntityType` -> `UpdateEncounterBossData`). `MonsterRank`,
/// which the previous version of this comment cited as the reason to hand-curate
/// this list instead, is `""` for every one of the table's 3094 shipped rows —
/// a dead, unshipped field, not a finer-grained elites-vs-bosses signal — and
/// the attrs that might look like a substitute (`AttrIsMonsterRankEnable` =
/// 459, `AttrMonsterRank` = 460) are never read by the reference tool's IL and
/// have no enum giving their values meaning. Do not resurrect either as a
/// classification source.
///
/// A short manual-override list in `scripts/gen-name-tables.py`
/// (`BOSS_ID_MANUAL_OVERRIDES`) adds back ids the previous hand-curated list
/// carried that `MonsterType` does not mark as 2 but that community trackers
/// fought and flagged as bosses, plus ids whose `MonsterType` is demonstrably
/// stale (issue #313's World Dominator rotation); see its per-id comments.
///
/// Previously hand-curated from `crates/meter/data/MonsterNameBoss.json`
/// (community-tracker data, GPL-3.0); see `THIRD_PARTY_NOTICES.md` for the
/// licence of the table this now derives from instead.
///
/// This set is deliberately *not* widened by issue #36's authoritative
/// backfill beyond what `MonsterType == 2` already yields: `monster_name`
/// covers an order of magnitude more ids than are bosses, and having a name
/// must never by itself imply bosshood.
///
/// Sorted ascending; `is_boss_monster` binary-searches it."""

# rustfmt's own array-literal packing: greedily fill each line up to this
# width (indent included) before wrapping, matching the `#[rustfmt::skip]`
# formatting the array was originally written with so a diff against a
# hand-formatted file stays empty.
_BOSS_IDS_LINE_WIDTH = 96
_BOSS_IDS_INDENT = "    "


def _emit_wrapped_u32_array(
    out: io.StringIO,
    doc: str,
    const_name: str,
    ids: list[int],
    line_width: int,
    indent: str,
    predicate_fn: str,
) -> None:
    """Writes `doc`, a `#[rustfmt::skip]` `const {const_name}: &[u32]`
    greedily line-wrapped at `line_width` (matching rustfmt's own
    array-literal packing so a diff against a hand-formatted file stays
    empty), and `predicate_fn` after it. Shared by `emit_boss_ids` and
    `emit_dungeon_scene_ids`, which differ only in the args they pass."""
    out.write(f"\n{doc}\n#[rustfmt::skip]\nconst {const_name}: &[u32] = &[\n")
    line = indent
    for n in ids:
        piece = f"{n}, "
        if line != indent and len(line) + len(piece) > line_width:
            out.write(line.rstrip() + "\n")
            line = indent
        line += piece
    if line != indent:
        out.write(line.rstrip() + "\n")
    out.write("];\n")
    out.write(predicate_fn)


def emit_boss_ids(out: io.StringIO, ids: list[int]) -> None:
    _emit_wrapped_u32_array(
        out,
        BOSS_IDS_DOC,
        "BOSS_MONSTER_IDS",
        ids,
        _BOSS_IDS_LINE_WIDTH,
        _BOSS_IDS_INDENT,
        "\n/// Whether `id` is a known boss-monster template id (issue #42) — i.e.\n"
        "/// whether the encounter name should ever be surfaced for it.\n"
        "pub fn is_boss_monster(id: u32) -> bool {\n"
        "    BOSS_MONSTER_IDS.binary_search(&id).is_ok()\n"
        "}\n",
    )


DUNGEON_SCENE_IDS_DOC = """/// Dungeon scene ids (issue #125): every scene id `DungeonsTable.json` lists
/// as a dungeon instance's `SceneID`, plus the short manual-override list in
/// `scripts/gen-name-tables.py` (`DUNGEON_SCENE_ID_MANUAL_OVERRIDES`) for
/// instances that column omits — 573 distinct ids (min 1001, max
/// 171001). No upstream table maps a scene/dungeon to its final boss (issue
/// #125's investigation checked every `BPSR-ZDPS/Data/*.json` table and both
/// community data repos), which is why [`SCENE_FINAL_BOSSES`] is curated by
/// hand instead (issue #201). This table is the wider "is this an instance at
/// all" answer, used by `Meter::apply`'s scene-change handling
/// (`crates/meter/src/encounter.rs`) and by issue #151's in-dungeon fight
/// hold, neither of which should treat an open-world town or field as a
/// dungeon.
///
/// Generated from `crates/meter/data/DungeonSceneIds.json`, itself filtered
/// from BPSR-ZDPS's `DungeonsTable.json` — the same URL vendored as
/// `DungeonsTableNames.json`, just projected to scene ids instead of names.
///
/// Sorted ascending; `is_dungeon_scene` binary-searches it."""

# Mirrors `_BOSS_IDS_LINE_WIDTH`/`_BOSS_IDS_INDENT` above — see their comment.
_DUNGEON_SCENE_IDS_LINE_WIDTH = 96
_DUNGEON_SCENE_IDS_INDENT = "    "


def emit_dungeon_scene_ids(out: io.StringIO, ids: list[int]) -> None:
    _emit_wrapped_u32_array(
        out,
        DUNGEON_SCENE_IDS_DOC,
        "DUNGEON_SCENE_IDS",
        ids,
        _DUNGEON_SCENE_IDS_LINE_WIDTH,
        _DUNGEON_SCENE_IDS_INDENT,
        "\n/// Whether `id` is a known dungeon scene id (issue #125) — i.e. whether\n"
        "/// the party is inside a dungeon instance rather than an open-world\n"
        "/// town or field.\n"
        "pub fn is_dungeon_scene(id: u32) -> bool {\n"
        "    DUNGEON_SCENE_IDS.binary_search(&id).is_ok()\n"
        "}\n",
    )


SCENE_FINAL_BOSSES_DOC = """/// Curated dungeon scene -> final-boss monster id (issue #201).
///
/// Hand-maintained in `crates/meter/data/SceneFinalBosses.json` — nothing
/// upstream carries this mapping (issue #125's investigation checked every
/// `BPSR-ZDPS/Data/*.json` table and both community data repos), and issue
/// #201 replaced issue #131's runtime learning of it with a written-down
/// table: there are few enough dungeons in the game that learning one boss per
/// scene at runtime — and shipping an "I don't trust the cache" reset button
/// to go with it — cost more than curating them.
///
/// **Single-boss dungeons only.** This is what `Meter::snapshot` puts in
/// `EncounterInfo::scene_boss_name`, i.e. the header caption *before* — or
/// without — a boss hit. The moment a recognized boss is actually engaged the
/// live lock in `Meter::recompute_boss` takes over and `encounter_title`
/// (`crates/app/src/ui.rs`) prefers it, so a multi-boss dungeon has nothing to
/// gain here and everything to get wrong. Raid scenes
/// (`phase::is_boss_select_scene`) are suppressed outright and must never
/// appear.
///
/// A dungeon absent from this table simply has no pre-pull caption; that is
/// the expected state for most of the 572 ids `is_dungeon_scene` knows.
///
/// Sorted by scene id; `scene_final_boss` binary-searches it."""

# Mirrors `_BOSS_IDS_LINE_WIDTH`/`_BOSS_IDS_INDENT` above — see their comment.
_SCENE_FINAL_BOSSES_INDENT = "    "


def emit_scene_final_bosses(
    out: io.StringIO, pairs: list[tuple[int, int]], scenes: dict[int, str], monsters: dict[int, str]
) -> None:
    """One pair per line, each annotated with the names — the table is small
    and hand-curated, so a reviewer reading `tables.rs` should not have to go
    look two ids up to see what an entry claims."""
    out.write(f"\n{SCENE_FINAL_BOSSES_DOC}\n#[rustfmt::skip]\n")
    out.write("pub(crate) const SCENE_FINAL_BOSSES: &[(u32, u32)] = &[\n")
    for scene, boss in pairs:
        indent = _SCENE_FINAL_BOSSES_INDENT
        out.write(f"{indent}({scene}, {boss}), // {scenes[scene]} -> {monsters[boss]}\n")
    out.write("];\n")
    out.write(
        "\n/// The curated final boss of dungeon scene `id` (issue #201), if it is a\n"
        "/// single-boss dungeon someone has written down. See\n"
        "/// [`SCENE_FINAL_BOSSES`].\n"
        "pub fn scene_final_boss(id: u32) -> Option<u32> {\n"
        "    SCENE_FINAL_BOSSES\n"
        "        .binary_search_by_key(&id, |&(scene, _)| scene)\n"
        "        .ok()\n"
        "        .map(|i| SCENE_FINAL_BOSSES[i].1)\n"
        "}\n"
    )


@functools.lru_cache(maxsize=None)
def _fetch(url: str) -> bytes:
    # Cached by URL: `BOSS_ID_SOURCES["MonsterTableBossIds.json"]` and
    # `FILTERED_SOURCES["MonsterTableNames.json"]` are the same upstream URL,
    # projected two different ways, so without this a `--refresh` would
    # download the multi-megabyte `MonsterTable.json` twice.
    with urllib.request.urlopen(url, timeout=120) as resp:  # noqa: S310 - fixed https URLs
        return resp.read()


def _render_source(name: str, url: str) -> bytes:
    """Download `url` and render it exactly as it should be vendored as `name`."""
    body = _fetch(url)
    if name in FILTERED_SOURCES:
        filtered = filter_id_names(json.loads(body.decode("utf-8-sig")))
        return (json.dumps(filtered, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    if name in ICON_SOURCES:
        icons = filter_id_icons(json.loads(body.decode("utf-8-sig")))
        return (json.dumps(icons, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    if name in BOSS_ID_SOURCES:
        ids = filter_boss_ids(json.loads(body.decode("utf-8-sig")))
        return (json.dumps(ids, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    if name in DUNGEON_SCENE_ID_SOURCES:
        ids = filter_dungeon_scene_ids(json.loads(body.decode("utf-8-sig")))
        return (json.dumps(ids, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    # `VERBATIM_SOURCES` are vendored byte-for-byte, but a leading UTF-8 BOM
    # would otherwise ride along untouched: strip it here so every vendored
    # file — filtered or verbatim — is guaranteed BOM-free on disk, matching
    # what `render()`'s loader is written to expect.
    return body.removeprefix(b"\xef\xbb\xbf")


def refresh(check: bool) -> bool:
    """Re-download every source. Returns True when everything is in sync.

    With `check`, nothing is written: each freshly rendered source is compared
    against the vendored copy and drift is reported. Without it, the vendored
    copies are overwritten.
    """
    clean = True
    for name, url in {
        **VERBATIM_SOURCES,
        **FILTERED_SOURCES,
        **ICON_SOURCES,
        **BOSS_ID_SOURCES,
        **DUNGEON_SCENE_ID_SOURCES,
    }.items():
        rendered = _render_source(name, url)
        target = DATA / name
        current = target.read_bytes() if target.exists() else None
        if current == rendered:
            print(f"  up to date: {name}")
            continue
        clean = False
        if check:
            was = "missing" if current is None else f"{len(current)} bytes"
            print(f"  DRIFT: {name} ({was} vendored, {len(rendered)} bytes upstream)")
        else:
            target.write_bytes(rendered)
            print(f"  updated: {name}")
    return clean


def render() -> str:
    """Build the full text of `tables.rs` from the vendored JSON."""
    # `utf-8-sig` tolerates a leading UTF-8 BOM (and is a no-op without one),
    # so a BOM'd vendored file can never crash `render()` with a bare
    # `json.JSONDecodeError` instead of a clear drift message. `_render_source`
    # already strips the BOM on write, so this is defense in depth.
    load = lambda name: json.loads((DATA / name).read_text(encoding="utf-8-sig"))  # noqa: E731

    # Least authoritative layer first in every call below.
    monsters = merge_names(
        _keyed(load("MonsterTableNames.json")),
        _keyed(load("MonsterName.json")),
        _keyed(load("MonsterNameCrowdsource.json")),
        # Issue #313: the final word, above even the curated community layer.
        # See `MONSTER_NAME_MANUAL_OVERRIDES` for why a handful of ids need
        # the client's own name back.
        _keyed(MONSTER_NAME_MANUAL_OVERRIDES),
    )
    scenes = merge_names(
        # `DungeonsTableNames` is currently a strict id-subset of
        # `SceneTableNames` carrying coarser names — "Tina's Mindrealm" where
        # the scene table distinguishes "Chaotic - Tina's Mindrealm" — so it
        # ranks lowest and today contributes nothing. It stays wired up so an
        # id upstream adds to dungeons alone is still picked up on refresh.
        _keyed(load("DungeonsTableNames.json")),
        _keyed(load("SceneTableNames.json")),
        _keyed(load("SceneName.json")),
    )
    boss_ids = sorted(
        {int(x) for x in load("MonsterTableBossIds.json")} | set(BOSS_ID_MANUAL_OVERRIDES)
    )
    dungeon_scene_ids = sorted(
        {int(x) for x in load("DungeonSceneIds.json")}
        | set(DUNGEON_SCENE_ID_MANUAL_OVERRIDES)
    )
    scene_final_bosses = filter_scene_final_bosses(
        load(SCENE_FINAL_BOSSES_FILE),
        scenes,
        monsters,
        set(boss_ids),
        set(dungeon_scene_ids),
    )
    # Issue #16: inverted precedence vs. monsters/scenes above — here the
    # bulk community dump (`SkillName.json`) is the backfill layer and
    # BPSR-ZDPS's curated `SkillOverridesNames.json` is the hand-checked
    # layer, so it goes last (least authoritative first, `merge_names` lets
    # later layers win). See `HEADER` for the full rationale.
    skills = merge_names(
        _keyed(load("SkillName.json")),
        _keyed(load("SkillOverridesNames.json")),
    )
    # Issue #192: the icon half of the same curated rows, backfilled from the
    # full client table by issue #247. Same precedence as `skills` above and
    # for the same reason: `SkillTableIcons.json` is the bulk layer, the
    # curated `SkillOverridesIcons.json` is hand-checked, so it goes last and
    # wins where both name an icon for an id.
    skill_icons = merge_names(
        _keyed(load("SkillTableIcons.json")),
        _keyed(load("SkillOverridesIcons.json")),
    )

    out = io.StringIO()
    out.write(HEADER)
    emit(
        out,
        "/// Display name for a monster template id, if either name table names it.",
        "monster_name",
        monsters,
    )
    emit(
        out,
        "/// Display name for a scene (map/instance) id, if either name table names it.",
        "scene_name",
        scenes,
    )
    emit(
        out,
        "/// Display name for a skill id, if either name table names it.",
        "skill_name",
        skills,
    )
    emit(out, SKILL_ICON_DOC, "skill_icon", skill_icons)
    emit_boss_ids(out, boss_ids)
    emit_dungeon_scene_ids(out, dungeon_scene_ids)
    emit_scene_final_bosses(out, scene_final_bosses, scenes, monsters)
    out.write(FOOTER)
    print(
        f"{len(monsters)} monsters, {len(scenes)} scenes, {len(skills)} skills, "
        f"{len(boss_ids)} boss ids, {len(dungeon_scene_ids)} dungeon scene ids, "
        f"{len(scene_final_bosses)} curated scene final bosses",
        file=sys.stderr,
    )
    return out.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="re-download every source table from upstream before generating",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="write nothing; exit 1 if the generated file (or, with --refresh, "
        "any vendored source) is out of date",
    )
    args = parser.parse_args()

    _self_test()

    clean = True
    if args.refresh:
        print("checking sources against upstream:" if args.check else "refreshing sources:")
        clean = refresh(args.check)

    rendered = render()
    if args.check:
        current = OUT.read_text(encoding="utf-8") if OUT.exists() else None
        if current != rendered:
            clean = False
            print(f"DRIFT: {OUT} is out of date; re-run scripts/gen-name-tables.py")
        if not clean:
            print("\nrun `python3 scripts/gen-name-tables.py --refresh` and commit the result")
            sys.exit(1)
        print("name tables are up to date")
        return

    OUT.write_text(rendered, encoding="utf-8")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
