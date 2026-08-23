//! Reader for BPSR-ZDPS's non-protobuf "blob" wire format (issue #139),
//! carried inside `SyncDungeonDirtyData.v_data.buffer`
//! (`decode::opcode::SYNC_DUNGEON_DIRTY_DATA`) and, unhandled by this crate
//! on purpose, `SyncContainerDirtyData`'s `diff_blob` (see `decode.rs`'s
//! comment on that opcode). Ported from `BPSR-ZDPSLib/BlobReader.cs` +
//! `Blobs/BlobType.cs`: a little-endian i32 tree keyed by sentinel words
//! (`-2` begin, `-3` end, `-4` empty list/hashmap, `-1` hashmap "new
//! value").
//!
//! **The one porting correction that matters** (see the module-level spec,
//! `docs/specs/2026-08-23-issue-139-dungeon-state-spec.md`): ZDPS derives
//! whether a blob is padded with `0xDEADBEEF` guard words from the wrapping
//! `BufferStream.stream_type`. On this build that field is always `0` even
//! though every one of 392 real capture payloads *is* padded — so
//! [`detect_stream_safe`] reads the padding straight off the buffer
//! instead, the same class of correction the disproven
//! `CharSerialize.scene_data` port needed (issue #35/#111).
//!
//! Every read here is bounds-checked and returns `Option`/`Result` —
//! nothing in this module panics, ever, on any input. A short, truncated,
//! or otherwise malformed buffer simply fails to parse; `decode.rs` drops
//! the message with a `log::debug!` in that case, matching every other
//! decode failure in this crate.

/// Struct begin sentinel (`0xFFFF_FFFE` as `i32`).
const BEGIN: i32 = -2;
/// Struct end sentinel (`0xFFFF_FFFD` as `i32`).
const END: i32 = -3;
/// "Empty" sentinel shared by `list.count` and `hashmap.add`.
const EMPTY: i32 = -4;
/// Hashmap `add` sentinel meaning "this is a single new value, not an
/// add/remove/update triple" — the remove/update counts are not present on
/// the wire at all in this case.
const HASHMAP_NEW_VALUE: i32 = -1;

/// Guard word ZDPS pads every read with on a stream-safe blob.
const GUARD_LEN: usize = 4;

/// True iff `buf` is padded with `0xDEADBEEF` guard words between reads.
///
/// Verified against all 392 real `SyncDungeonDirtyData` payloads in this
/// build's captures: `buf[4..8] == 0xDEAD_BEEF` iff the blob is padded, so
/// no message was ever missed or misread by this rule. Byte `0..4` is
/// always the struct's `-2` begin tag; a stream-safe blob puts its first
/// guard word immediately after that.
pub fn detect_stream_safe(buf: &[u8]) -> bool {
    buf.len() >= 8 && u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) == 0xDEAD_BEEF
}

/// Cursor over one blob buffer. See the module doc for the wire format.
pub struct BlobReader<'a> {
    buf: &'a [u8],
    pos: usize,
    stream_safe: bool,
}

/// Result of a hashmap read: `add`/`update` carry full `(key, value)`
/// pairs, `remove` carries bare keys. Per the spec, both `add` and
/// `update` can carry live data on this build (an already-tracked
/// objective's completion arrives as an `update`, not an `add`) — callers
/// must apply both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashmapDelta<K, V> {
    pub add: Vec<(K, V)>,
    pub remove: Vec<K>,
    pub update: Vec<(K, V)>,
}

// Hand-written rather than `#[derive(Default)]`: the derive would add
// spurious `K: Default, V: Default` bounds even though every field here is
// just an empty `Vec`, which needs no bound on its element type at all.
impl<K, V> Default for HashmapDelta<K, V> {
    fn default() -> Self {
        Self {
            add: Vec::new(),
            remove: Vec::new(),
            update: Vec::new(),
        }
    }
}

/// Whether a struct-field callback recognized the field index it was
/// handed. `Unknown` makes [`BlobReader::read_struct`] abandon the rest of
/// the struct — ZDPS's own behavior (`BlobType.Read`): the field's value
/// is skipped by jumping straight to the struct's end sentinel rather than
/// attempting to read the wire past a field this reader doesn't model.
pub enum Field {
    Handled,
    Unknown,
}

impl<'a> BlobReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            stream_safe: detect_stream_safe(buf),
        }
    }

    pub fn is_stream_safe(&self) -> bool {
        self.stream_safe
    }

    /// Reads one little-endian `i32`, skipping the trailing `0xDEADBEEF`
    /// guard word when the blob is stream-safe. Bounds-checked: `None` on
    /// a truncated buffer, never a panic.
    pub fn read_i32(&mut self) -> Option<i32> {
        let end = self.pos.checked_add(4)?;
        let bytes: [u8; 4] = self.buf.get(self.pos..end)?.try_into().ok()?;
        self.pos = end;
        if self.stream_safe {
            let gend = self.pos.checked_add(GUARD_LEN)?;
            if gend > self.buf.len() {
                return None;
            }
            self.pos = gend;
        }
        Some(i32::from_le_bytes(bytes))
    }

    /// Reads `u32(len) utf8[len]`. Per the wire format, the length word and
    /// the raw content bytes are each their own "read" — on a stream-safe
    /// blob both are independently followed by a guard word.
    pub fn read_string(&mut self) -> Option<String> {
        let len = self.read_i32()?;
        let len = usize::try_from(len).ok()?;
        let end = self.pos.checked_add(len)?;
        let bytes = self.buf.get(self.pos..end)?;
        let s = std::str::from_utf8(bytes).ok()?.to_string();
        self.pos = end;
        if self.stream_safe {
            let gend = self.pos.checked_add(GUARD_LEN)?;
            if gend > self.buf.len() {
                return None;
            }
            self.pos = gend;
        }
        Some(s)
    }

    /// Reads a struct's `i32(-2) i32(size)` header and returns the
    /// absolute buffer position where its body ends — `body_start + size`,
    /// i.e. exactly where the trailing `-3` end sentinel begins.
    fn struct_header(&mut self) -> Option<usize> {
        if self.read_i32()? != BEGIN {
            return None;
        }
        let size = usize::try_from(self.read_i32()?).ok()?;
        let body_end = self.pos.checked_add(size)?;
        if body_end > self.buf.len() {
            return None;
        }
        Some(body_end)
    }

    fn seek_to(&mut self, pos: usize) -> Option<()> {
        if pos > self.buf.len() {
            return None;
        }
        self.pos = pos;
        Some(())
    }

    /// Reads one `struct := i32(-2) i32(size) { i32(index>0) field }*
    /// i32(-3)`, calling `on_field(self, index)` for each field entry.
    /// `on_field` must fully consume the field's value before returning
    /// [`Field::Handled`]; returning [`Field::Unknown`] abandons the rest
    /// of the struct (seeks straight to the end sentinel) without reading
    /// anything more from it — the field's value is never touched.
    pub fn read_struct(
        &mut self,
        mut on_field: impl FnMut(&mut Self, i32) -> Option<Field>,
    ) -> Option<()> {
        let body_end = self.struct_header()?;
        loop {
            let idx = self.read_i32()?;
            if idx == END {
                return Some(());
            }
            match on_field(self, idx)? {
                Field::Handled => {}
                Field::Unknown => {
                    self.seek_to(body_end)?;
                    return if self.read_i32()? == END {
                        Some(())
                    } else {
                        None
                    };
                }
            }
        }
    }

    /// Reads `list<T> := i32(count) T*`, `count == -4` meaning empty.
    pub fn read_list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let count = self.read_i32()?;
        if count == EMPTY {
            return Some(Vec::new());
        }
        if count < 0 {
            return None;
        }
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(item(self)?);
        }
        Some(out)
    }

    /// Reads `hashmap<K,V> := i32(add) [i32(remove) i32(update)] (K V)*add
    /// K*remove (K V)*update`.
    ///
    /// `add == -4` returns an empty delta immediately. `add == -1` means
    /// "a single new value": re-read `add` and read that many `(K,V)`
    /// pairs with no remove/update section on the wire at all. Otherwise
    /// `remove`/`update` counts follow and every section is read.
    pub fn read_hashmap<K, V>(
        &mut self,
        mut read_key: impl FnMut(&mut Self) -> Option<K>,
        mut read_value: impl FnMut(&mut Self) -> Option<V>,
    ) -> Option<HashmapDelta<K, V>> {
        let mut add_n = self.read_i32()?;
        if add_n == EMPTY {
            return Some(HashmapDelta::default());
        }
        let (remove_n, update_n) = if add_n == HASHMAP_NEW_VALUE {
            add_n = self.read_i32()?;
            (0, 0)
        } else {
            (self.read_i32()?, self.read_i32()?)
        };
        if add_n < 0 || remove_n < 0 || update_n < 0 {
            return None;
        }
        let mut add = Vec::new();
        for _ in 0..add_n {
            let k = read_key(self)?;
            let v = read_value(self)?;
            add.push((k, v));
        }
        let mut remove = Vec::new();
        for _ in 0..remove_n {
            remove.push(read_key(self)?);
        }
        let mut update = Vec::new();
        for _ in 0..update_n {
            let k = read_key(self)?;
            let v = read_value(self)?;
            update.push((k, v));
        }
        Some(HashmapDelta {
            add,
            remove,
            update,
        })
    }
}

// -- `DungeonDirtyData` and friends (issue #139) ----------------------------
//
// Everything not listed here (BPSR-ZDPS's blob carries a lot more than
// this) is left unmodeled on purpose, per the spec: an unrecognized field
// index simply abandons the rest of whichever struct it appears in
// (`BlobReader::read_struct`'s `Field::Unknown` path).

/// `FlowInfo` (blob field 2 of `DungeonDirtyData`). Only `state` (field 1)
/// is decoded into something callers act on; fields 2..8 are read (so the
/// field-order-dependent "unknown field abandons the struct" rule doesn't
/// cut the struct short before `state`) but otherwise discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowInfo {
    pub state: i32,
}

/// `TargetData` (the hashmap value type nested in blob field 4's
/// `Target.map`). Every field is `Option`: an *update* entry commonly
/// omits `target_id` entirely, so the hashmap key — not this field — is
/// the authoritative target id (see `decode.rs`'s dungeon dirty-data
/// handling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TargetData {
    pub target_id: Option<i32>,
    pub nums: Option<i32>,
    pub complete: Option<i32>,
}

/// `VarData` (the list item type nested in blob field 10's
/// `DungeonVar.list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarData {
    pub name: String,
    pub value: i32,
}

/// The top-level blob carried by `SyncDungeonDirtyData.v_data.buffer`. Each
/// field is `Option`, matching the wire's dirty/delta nature — a given
/// message typically carries only one or two of these (the real capture
/// fixtures each carry exactly one).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DungeonDirtyData {
    pub scene_uuid: Option<u32>,
    pub flow_info: Option<FlowInfo>,
    pub target: Option<HashmapDelta<i32, TargetData>>,
    pub dungeon_var: Option<Vec<VarData>>,
}

fn parse_flow_info(r: &mut BlobReader<'_>) -> Option<FlowInfo> {
    let mut state = None;
    r.read_struct(|r, idx| match idx {
        1 => {
            state = Some(r.read_i32()?);
            Some(Field::Handled)
        }
        // Decoded so a later real field of `FlowInfo` isn't wrongly
        // treated as unknown-and-abandoned, but not otherwise interpreted
        // (spec: "2..8: i32 (decoded, unused)").
        2..=8 => {
            r.read_i32()?;
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(FlowInfo { state: state? })
}

fn parse_target_data(r: &mut BlobReader<'_>) -> Option<TargetData> {
    let mut data = TargetData::default();
    r.read_struct(|r, idx| match idx {
        1 => {
            data.target_id = Some(r.read_i32()?);
            Some(Field::Handled)
        }
        2 => {
            data.nums = Some(r.read_i32()?);
            Some(Field::Handled)
        }
        3 => {
            data.complete = Some(r.read_i32()?);
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(data)
}

fn parse_target(r: &mut BlobReader<'_>) -> Option<HashmapDelta<i32, TargetData>> {
    let mut delta = None;
    r.read_struct(|r, idx| match idx {
        1 => {
            delta = Some(r.read_hashmap(|r| r.read_i32(), parse_target_data)?);
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(delta.unwrap_or_default())
}

fn parse_var_data(r: &mut BlobReader<'_>) -> Option<VarData> {
    let mut name = None;
    let mut value = None;
    r.read_struct(|r, idx| match idx {
        1 => {
            name = Some(r.read_string()?);
            Some(Field::Handled)
        }
        2 => {
            value = Some(r.read_i32()?);
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(VarData {
        name: name?,
        value: value?,
    })
}

fn parse_dungeon_var(r: &mut BlobReader<'_>) -> Option<Vec<VarData>> {
    let mut list = None;
    r.read_struct(|r, idx| match idx {
        1 => {
            list = Some(r.read_list(parse_var_data)?);
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(list.unwrap_or_default())
}

/// Parses `SyncDungeonDirtyData.v_data.buffer` (`decode::opcode::SYNC_DUNGEON_DIRTY_DATA`)
/// into a [`DungeonDirtyData`]. Returns `None` on any malformed or
/// truncated buffer — never panics; the caller drops the message with a
/// debug log, same as a prost decode failure elsewhere in this crate.
pub fn parse_dungeon_dirty_data(buf: &[u8]) -> Option<DungeonDirtyData> {
    let mut r = BlobReader::new(buf);
    let mut result = DungeonDirtyData::default();
    r.read_struct(|r, idx| match idx {
        1 => {
            result.scene_uuid = Some(r.read_i32()? as u32);
            Some(Field::Handled)
        }
        2 => {
            result.flow_info = Some(parse_flow_info(r)?);
            Some(Field::Handled)
        }
        4 => {
            result.target = Some(parse_target(r)?);
            Some(Field::Handled)
        }
        10 => {
            result.dungeon_var = Some(parse_dungeon_var(r)?);
            Some(Field::Handled)
        }
        _ => Some(Field::Unknown),
    })?;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dump_format::hex_decode;

    /// The raw `SyncDungeonDirtyData.v_data.buffer` bytes (i.e. the blob
    /// itself, *not* the outer `Notify.payload`) for the `FlowInfo.State =
    /// 4 (End)` real capture fixture — see
    /// `docs/specs/2026-08-23-issue-139-dungeon-state-spec.md`.
    /// `decode::tests` exercises the full `Notify.payload` (protobuf +
    /// blob together) for all four real fixtures; this module tests the
    /// blob reader in isolation.
    const REAL_FLOW_INFO_END_BLOB: &str = "feffffffefbeadde80000000efbeadde02000000efbeaddefeffffffefbeadde30000000efbeadde01000000efbeadde04000000efbeadde05000000efbeaddedecb836aefbeadde08000000efbeadde01000000efbeaddefdffffffefbeadde07000000efbeaddefeffffffefbeadde10000000efbeadde01000000efbeadde8f010000efbeaddefdffffffefbeaddefdffffffefbeadde";

    #[test]
    fn detects_stream_safe_on_a_real_padded_blob() {
        let buf = hex_decode(REAL_FLOW_INFO_END_BLOB).unwrap();
        assert!(detect_stream_safe(&buf));
        let data = parse_dungeon_dirty_data(&buf).unwrap();
        assert_eq!(data.flow_info.unwrap().state, 4);
    }

    /// Hand-built, no `0xDEADBEEF` guard words anywhere — covers the
    /// non-stream-safe padding branch the real fixtures (all padded) can't
    /// exercise. Shape: `DungeonDirtyData { 2: FlowInfo { 1: 3 } }`
    /// (`state = 3`, `Playing`).
    fn synthetic_non_stream_safe_blob() -> Vec<u8> {
        fn i32le(v: i32) -> [u8; 4] {
            v.to_le_bytes()
        }
        let flow_body = [i32le(1), i32le(3)].concat(); // field 1 (state) = 3
        let flow = [
            i32le(BEGIN).as_slice(),
            i32le(flow_body.len() as i32).as_slice(),
            flow_body.as_slice(),
            i32le(END).as_slice(),
        ]
        .concat();
        let outer_body = [i32le(2).as_slice(), flow.as_slice()].concat(); // field 2 (flow_info)
        [
            i32le(BEGIN).as_slice(),
            i32le(outer_body.len() as i32).as_slice(),
            outer_body.as_slice(),
            i32le(END).as_slice(),
        ]
        .concat()
    }

    #[test]
    fn detects_non_stream_safe_on_a_hand_built_unpadded_blob() {
        let buf = synthetic_non_stream_safe_blob();
        assert!(!detect_stream_safe(&buf));
        let data = parse_dungeon_dirty_data(&buf).unwrap();
        assert_eq!(data.flow_info.unwrap().state, 3);
    }

    #[test]
    fn truncated_buffer_never_panics_and_yields_none() {
        let full = hex_decode(REAL_FLOW_INFO_END_BLOB).unwrap();
        for cut in [0, 1, 4, 7, 8, 20, full.len() - 1] {
            let truncated = &full[..cut];
            // Must not panic for any truncation point.
            assert!(
                parse_dungeon_dirty_data(truncated).is_none(),
                "a truncated buffer of len {cut} must fail to parse, not succeed"
            );
        }
    }

    #[test]
    fn empty_buffer_never_panics_and_yields_none() {
        assert!(parse_dungeon_dirty_data(&[]).is_none());
        assert!(!detect_stream_safe(&[]));
    }
}
