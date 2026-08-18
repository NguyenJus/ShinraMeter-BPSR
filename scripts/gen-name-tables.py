#!/usr/bin/env python3
"""Regenerate `crates/meter/src/tables.rs` from the vendored name tables.

Two families of source feed these tables, and they are merged, not chosen
between (issue #36):

*Curated community tables* (GPL-3.0, the same licence as this project) from the
`resonance-logs` and `bpsr-logs` trackers. Small, hand-checked, and using the
names players actually say — "Rathalos", not the client's "Ignisor". Vendored
verbatim under `crates/meter/data/`.

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
}

# Authoritative game tables, vendored *filtered*. Upstream these are 6.5 MB,
# 0.6 MB and 1.1 MB of full row data; `filter_id_names` reduces each to the
# `{id: name}` pairs that are all this project wants from them.
FILTERED_SOURCES = {
    "MonsterTableNames.json": f"{_ZDPS}/MonsterTable.json",
    "SceneTableNames.json": f"{_ZDPS}/SceneTable.json",
    "DungeonsTableNames.json": f"{_ZDPS}/DungeonsTable.json",
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
}

HEADER = """//! Static id -> display-name tables for monsters and scenes.
//!
//! Generated by `scripts/gen-name-tables.py`. Do not edit by hand.
//!
//! Merged from two families of source under `crates/meter/data/` (issue #36):
//! the curated community tables shipped by the `resonance-logs` and
//! `bpsr-logs` trackers (GPL-3.0, the same licence as this project), and the
//! authoritative game tables from `Blue-Protocol-Source/BPSR-ZDPS` (MIT),
//! filtered down to their `{id: name}` pairs. See `THIRD_PARTY_NOTICES.md`.
//!
//! Where the two disagree the curated name wins: it is hand-checked and is
//! what players call the thing, while the client table carries the raw
//! internal name.
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
        // BPSR-ZDPS's `MonsterTable.json` calls template 1013 "Ignisor";
        // the community tables call it "Rathalos". The curated community name
        // is the one players actually use, so it must survive the backfill —
        // this is the precedence in `merge_names` observed end to end.
        assert_eq!(monster_name(1013), Some("Rathalos"));
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
    out.write(f"\n{doc}\npub fn {fn}(id: u32) -> Option<&'static str> {{\n")
    out.write("    Some(match id {\n")
    for key in sorted(table):
        out.write('        %d => "%s",\n' % (key, esc(table[key])))
    out.write("        _ => return None,\n    })\n}\n")


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
/// fought and flagged as bosses; see its comment for the one id it currently
/// carries.
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
/// as a dungeon instance's `SceneID` — 572 distinct ids (min 1001, max
/// 171001). No upstream table maps a scene/dungeon to its final boss (issue
/// #125's investigation checked every `BPSR-ZDPS/Data/*.json` table and both
/// community data repos), so `Meter::recompute_boss`
/// (`crates/meter/src/encounter.rs`) instead *learns* a dungeon's final boss
/// by observation — the last genuine boss engaged in a scene — and remembers
/// it per scene for the rest of the session. This table is what gates that
/// latch to actual dungeons: without it, killing a world boss in an
/// open-world zone would pin its name to the banner for every later visit to
/// that town or field.
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
        "/// the final-boss latch in `Meter::recompute_boss` may record a\n"
        "/// remembered boss for it.\n"
        "pub fn is_dungeon_scene(id: u32) -> bool {\n"
        "    DUNGEON_SCENE_IDS.binary_search(&id).is_ok()\n"
        "}\n",
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
    dungeon_scene_ids = sorted({int(x) for x in load("DungeonSceneIds.json")})

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
    emit_boss_ids(out, boss_ids)
    emit_dungeon_scene_ids(out, dungeon_scene_ids)
    out.write(FOOTER)
    print(
        f"{len(monsters)} monsters, {len(scenes)} scenes, {len(boss_ids)} boss ids, "
        f"{len(dungeon_scene_ids)} dungeon scene ids",
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
