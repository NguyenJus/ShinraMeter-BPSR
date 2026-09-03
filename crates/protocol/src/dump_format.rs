//! Shared JSONL dump-format parsing, used by both `bin/inspect-replay.rs`
//! and `bin/sanitize-dump.rs`. Reads the dump format written by
//! `crates/app/src/dump.rs` (see that module's doc comment for the
//! authoritative on-disk shape): one JSON object per line, hex-encoded
//! `service_uuid` / `method_id` / `payload_hex`, plus a `payload_decoded`
//! flag for a record the capture couldn't decompress (its `payload` is then
//! still-compressed bytes, not protobuf).
//!
//! Factored out of `inspect-replay.rs` (issue #25 slice B) rather than
//! duplicated, so the two binaries can't drift on line format, hex parsing,
//! or rotated-dump handling.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One dump record after parsing, decoupled from the on-disk hex-string
/// encoding — see `crates/app/src/dump.rs` for the JSON shape this comes
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpRecord {
    pub ts_ms: u64,
    pub service_uuid: u64,
    pub method_id: u32,
    pub payload: Vec<u8>,
    /// `false` when the capture couldn't decompress this fragment, so
    /// `payload` is the raw compressed bytes — a reader counts it in
    /// service/method histograms but must not hand it to the decoder.
    pub payload_decoded: bool,
}

/// The on-disk JSON shape, straight off the wire — see `crates/app/src/dump.rs`.
#[derive(Deserialize)]
struct RawLine {
    ts_ms: u64,
    service_uuid: String,
    method_id: String,
    payload_hex: String,
    payload_decoded: bool,
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn parse_hex_u32(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Hex-encodes `bytes` lowercase, no separators — the inverse of
/// [`hex_decode`], for a writer producing the same on-disk shape.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Formats one dump-format JSONL line (trailing `\n` included) for `record`.
pub fn format_line(record: &DumpRecord) -> String {
    format!(
        "{{\"ts_ms\":{},\"service_uuid\":\"0x{:016x}\",\"method_id\":\"0x{:08x}\",\"payload_hex\":\"{}\",\"payload_decoded\":{}}}\n",
        record.ts_ms,
        record.service_uuid,
        record.method_id,
        hex_encode(&record.payload),
        record.payload_decoded
    )
}

/// Parses one JSONL line of the dump format into a `DumpRecord`. `Err`
/// (never a panic) on malformed JSON, a non-`0x`-prefixed hex field, or an
/// odd-length `payload_hex` — the caller skips the line and keeps going
/// rather than aborting the whole run on one bad line.
pub fn parse_record(line: &str) -> Result<DumpRecord, String> {
    let raw: RawLine = serde_json::from_str(line).map_err(|err| err.to_string())?;
    let service_uuid = parse_hex_u64(&raw.service_uuid)
        .ok_or_else(|| format!("bad service_uuid: {}", raw.service_uuid))?;
    let method_id =
        parse_hex_u32(&raw.method_id).ok_or_else(|| format!("bad method_id: {}", raw.method_id))?;
    let payload = hex_decode(&raw.payload_hex)
        .ok_or_else(|| format!("bad payload_hex: {}", raw.payload_hex))?;
    Ok(DumpRecord {
        ts_ms: raw.ts_ms,
        service_uuid,
        method_id,
        payload,
        payload_decoded: raw.payload_decoded,
    })
}

/// `<path>.n` for `n >= 1` — the numbered ring chunk `dump.rs` would have
/// renamed `path` to `n` rotations ago (issue #322): `n=1` is the most
/// recently rotated (newest) chunk, higher `n` is older. The single
/// definition of the ring's naming scheme: `crates/app/src/dump.rs` (the
/// writer that creates these files) calls this one rather than keeping its
/// own copy, so the reader and the writer cannot drift.
pub fn numbered_sibling(path: &Path, n: u64) -> PathBuf {
    let mut numbered = path.as_os_str().to_owned();
    numbered.push(format!(".{n}"));
    PathBuf::from(numbered)
}

/// Every numbered ring chunk on disk next to `path`, ascending by `n`
/// (index 0 is `.1`, the newest rotated chunk). The single enumeration used
/// by both the writer's ring bookkeeping (`crates/app/src/dump.rs`) and
/// [`load_dump`] below.
///
/// Gap-tolerant *by scanning the directory* for `<file name>.N` siblings
/// rather than counting up from `.1` until a number is missing: a rotation
/// that shifted the ring and then failed to rename the live file leaves `.1`
/// vacant while `.2`, `.3`, ... still hold real data, and so does a chunk
/// deleted by hand. Counting up would stop at the hole and orphan
/// everything past it — never budgeted, never evicted, never replayed.
/// Entries whose suffix isn't a plain decimal `n >= 1` (`.1.bak`, `.01`,
/// `.gz`) are ignored, as is anything that isn't a regular file — a chunk
/// is always a file, and treating a same-named directory as one would have
/// the writer try to shift it through the ring.
pub fn ring_siblings(path: &Path) -> Vec<(u64, PathBuf)> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    let mut chunks: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            if !entry.file_type().is_ok_and(|ty| ty.is_file()) {
                return None;
            }
            let entry_name = entry.file_name().into_string().ok()?;
            let suffix = entry_name.strip_prefix(&prefix)?;
            let n: u64 = suffix.parse().ok()?;
            // Rejects `.0` and any non-canonical spelling (`.01`) that would
            // otherwise alias an existing chunk number.
            (n >= 1 && suffix == n.to_string()).then(|| (n, entry.path()))
        })
        .collect();
    chunks.sort_by_key(|(n, _)| *n);
    chunks
}

/// Opens `path`, parses every JSONL line with [`parse_record`], and appends
/// the successfully-parsed records to `records` in file order. A malformed
/// line is skipped (printed to stderr, `path` prefixed so it's clear which
/// file it came from) rather than aborting the whole run. `Err` only when
/// `path` itself can't be opened — the caller decides whether that's fatal.
/// Returns how many records were appended, for the caller to report.
pub fn append_records_from(
    path: &Path,
    records: &mut Vec<DumpRecord>,
) -> Result<usize, std::io::Error> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut appended = 0;
    for (lineno, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(err) => {
                eprintln!("{}: line {}: read error: {err}", path.display(), lineno + 1);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_record(&line) {
            Ok(r) => {
                records.push(r);
                appended += 1;
            }
            Err(err) => eprintln!(
                "{}: line {}: skipping malformed record: {err}",
                path.display(),
                lineno + 1
            ),
        }
    }
    Ok(appended)
}

/// Reads `path`, plus every numbered ring chunk present alongside it
/// (`.1`, `.2`, ... — see [`ring_siblings`], which tolerates a gap in the
/// numbering), in chronological order:
/// highest-numbered (oldest) chunk first, `path` (the live, newest data)
/// last. Convenience wrapper around [`append_records_from`] for a CLI
/// binary that just wants "every retained record for this dump, oldest
/// first". A rotated chunk's read failure is reported to stderr but not
/// fatal (older data is nice-to-have, not required); a failure to open
/// `path` itself is returned.
pub fn load_dump(path: &Path) -> Result<Vec<DumpRecord>, std::io::Error> {
    let mut records = Vec::new();

    for (n, chunk) in ring_siblings(path).into_iter().rev() {
        match append_records_from(&chunk, &mut records) {
            Ok(count) => eprintln!(
                "note: also read rotated dump chunk {} (.{n}) — {count} earlier record(s) \
                 included before {}",
                chunk.display(),
                path.display()
            ),
            Err(err) => eprintln!(
                "warning: found rotated dump chunk {} but could not read it ({err}); \
                 earlier records may be missing",
                chunk.display()
            ),
        }
    }

    append_records_from(path, &mut records)?;
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_line(ts_ms: u64) -> String {
        format!(
            r#"{{"ts_ms":{ts_ms},"service_uuid":"0x0000000063335342","method_id":"0x0000002d","payload_hex":"","payload_decoded":true}}"#
        )
    }

    #[test]
    fn hex_encode_decode_roundtrip() {
        let bytes = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn format_line_parses_back_to_the_same_record() {
        let record = DumpRecord {
            ts_ms: 42,
            service_uuid: 0x63335342,
            method_id: 0x2d,
            payload: vec![1, 2, 3],
            payload_decoded: true,
        };
        let line = format_line(&record);
        let parsed = parse_record(line.trim_end()).expect("well-formed line must parse");
        assert_eq!(parsed, record);
    }

    #[test]
    fn numbered_sibling_appends_dot_n_to_the_path() {
        let path = PathBuf::from("/some/dir/dump-123.jsonl");
        assert_eq!(
            numbered_sibling(&path, 1),
            PathBuf::from("/some/dir/dump-123.jsonl.1")
        );
        assert_eq!(
            numbered_sibling(&path, 2),
            PathBuf::from("/some/dir/dump-123.jsonl.2")
        );
    }

    #[test]
    fn load_dump_reads_rotated_sibling_before_the_live_file() {
        let dir = std::env::temp_dir();
        let n = std::process::id();
        let path = dir.join(format!("bpsr-dump-format-test-{n}.jsonl"));
        let rotated = numbered_sibling(&path, 1);
        std::fs::write(
            &rotated,
            format!("{}\n{}\n", dump_line(100), dump_line(200)),
        )
        .unwrap();
        std::fs::write(&path, format!("{}\n", dump_line(300))).unwrap();

        let records = load_dump(&path).expect("live file must open");

        assert_eq!(
            records.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    /// Issue #322: the whole numbered ring, not just a single `.1`, is read
    /// — and in chronological order (highest number = oldest, read first).
    #[test]
    fn load_dump_reads_every_numbered_ring_chunk_in_chronological_order() {
        let dir = std::env::temp_dir();
        let n = std::process::id();
        let path = dir.join(format!("bpsr-dump-format-test-ring-{n}.jsonl"));
        let dot1 = numbered_sibling(&path, 1);
        let dot2 = numbered_sibling(&path, 2);
        std::fs::write(&dot2, format!("{}\n", dump_line(100))).unwrap();
        std::fs::write(&dot1, format!("{}\n", dump_line(200))).unwrap();
        std::fs::write(&path, format!("{}\n", dump_line(300))).unwrap();

        let records = load_dump(&path).expect("live file must open");

        assert_eq!(
            records.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
            vec![100, 200, 300],
            ".2 (oldest) must be read before .1, and .1 before the live file"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&dot1);
        let _ = std::fs::remove_file(&dot2);
    }

    /// A hole in the numbering (a rotation that shifted the ring and then
    /// failed to rename the live file into `.1`, or a chunk deleted by
    /// hand) must not hide every older chunk behind it: enumeration scans
    /// the directory instead of counting up until a number is missing.
    #[test]
    fn ring_siblings_enumerates_past_a_missing_number() {
        let dir = std::env::temp_dir();
        let n = std::process::id();
        let path = dir.join(format!("bpsr-dump-format-test-gap-{n}.jsonl"));
        let dot1 = numbered_sibling(&path, 1);
        let dot2 = numbered_sibling(&path, 2);
        let dot3 = numbered_sibling(&path, 3);
        for chunk in [&dot1, &dot2, &dot3] {
            std::fs::write(chunk, format!("{}\n", dump_line(1))).unwrap();
        }
        std::fs::remove_file(&dot1).unwrap();

        let chunks = ring_siblings(&path);

        assert_eq!(
            chunks,
            vec![(2, dot2.clone()), (3, dot3.clone())],
            ".2 and .3 must still be enumerated, ascending, with .1 gone"
        );

        let _ = std::fs::remove_file(&dot2);
        let _ = std::fs::remove_file(&dot3);
    }

    /// The reader side of the same gap: `load_dump` must still fold the
    /// orphaned chunks in, oldest first, rather than reading only the live
    /// file.
    #[test]
    fn load_dump_reads_chunks_past_a_missing_number() {
        let dir = std::env::temp_dir();
        let n = std::process::id();
        let path = dir.join(format!("bpsr-dump-format-test-gap-load-{n}.jsonl"));
        let dot2 = numbered_sibling(&path, 2);
        let dot3 = numbered_sibling(&path, 3);
        std::fs::write(&dot3, format!("{}\n", dump_line(100))).unwrap();
        std::fs::write(&dot2, format!("{}\n", dump_line(200))).unwrap();
        std::fs::write(&path, format!("{}\n", dump_line(300))).unwrap();

        let records = load_dump(&path).expect("live file must open");

        assert_eq!(
            records.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
            vec![100, 200, 300],
            "a vacant .1 must not stop the reader at the live file"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&dot2);
        let _ = std::fs::remove_file(&dot3);
    }
}
