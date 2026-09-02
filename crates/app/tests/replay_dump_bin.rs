//! System test for the `replay-dump` binary (`src/bin/replay-dump.rs`):
//! runs it as a real subprocess against the same sanitized real-capture
//! fixture `replay_dump.rs` drives through `Rig::feed_notify` directly, and
//! checks its stdout carries the `encounter:` lifecycle narrative a live
//! session's log would have — specifically that the fight it replays is
//! seen to end, since that is the one transition `TAIL_MARGIN_MS`'s
//! trailing synthetic ticks exist to guarantee fires during a replay with
//! no live wall clock behind it.
//!
//! `dump_format::load_dump` (which the binary calls) reads a path on disk,
//! not the `.zst`-compressed fixture directly — `replay_dump.rs`'s own
//! module doc comment explains the fixture's provenance and decompression
//! (`zstd::stream::decode_all`); this test does the same decompression, to
//! a temp file, before invoking the binary as a subprocess.

use std::path::Path;
use std::process::Command;

/// Same fixture `replay_dump.rs` uses — see that file's module doc comment
/// for its provenance (a sanitized ~209s window of one real boss fight,
/// monster id 1152 "Kartgriff") and known shape.
const FIXTURE: &str = "/tests/fixtures/dump-2976-boss-fight.jsonl.zst";

fn decompress_fixture_to_temp() -> std::path::PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE.trim_start_matches('/'));
    let compressed = std::fs::read(&path).expect("read fixture");
    let jsonl = zstd::stream::decode_all(compressed.as_slice()).expect("decompress fixture");

    let dest = std::env::temp_dir().join(format!(
        "ShinraMeter-BPSR-replay-dump-bin-test-{}.jsonl",
        std::process::id()
    ));
    std::fs::write(&dest, jsonl).expect("write decompressed fixture");
    dest
}

#[test]
fn replay_dump_binary_prints_encounter_lines_and_a_fight_end() {
    let dump_path = decompress_fixture_to_temp();

    let output = Command::new(env!("CARGO_BIN_EXE_replay-dump"))
        .arg(&dump_path)
        .arg("--snapshot-at-end")
        .output()
        .expect("run the replay-dump binary");

    let _ = std::fs::remove_file(&dump_path);

    assert!(
        output.status.success(),
        "replay-dump exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");

    let encounter_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("encounter:"))
        .collect();
    assert!(
        !encounter_lines.is_empty(),
        "expected at least one encounter: line; got stdout:\n{stdout}"
    );
    assert!(
        encounter_lines
            .iter()
            .any(|line| line.contains("fight ended")),
        "expected a fight-end line among the replayed encounter: lines; got:\n{}",
        encounter_lines.join("\n")
    );

    // `--snapshot-at-end` must have printed the final rows too, one line
    // per player, each carrying a uid and a damage figure.
    assert!(
        stdout.contains("--- snapshot at ts_ms="),
        "expected the --snapshot-at-end footer; got stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("uid=") && stdout.contains("damage="),
        "expected at least one snapshot row; got stdout:\n{stdout}"
    );
}
