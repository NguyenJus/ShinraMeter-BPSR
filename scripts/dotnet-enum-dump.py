#!/usr/bin/env python3
"""Dump enums (and other metadata) out of a .NET assembly or single-file bundle.

Why this exists
---------------
Reference tools for Blue Protocol: Star Resonance ship as .NET "single-file"
executables: a native apphost with the managed assemblies appended verbatim.
The game's protocol constants (attribute ids, opcodes, monster ranks, ...) live
in those assemblies as C# enums.  An enum member is not code -- it is pure
metadata -- so it can be recovered without disassembling a single IL byte.

How the walk works (ECMA-335, 6th ed.)
--------------------------------------
1.  Locate every CLI metadata root by scanning the whole file for the ASCII
    signature ``BSJB`` (0x424A5342).  In a single-file bundle each embedded
    managed assembly contributes one root.  Because every stream offset in the
    metadata root is *relative to the root itself*, we never have to parse the
    PE headers or map RVAs -- the root is self-describing.
    Root layout: signature(4) major(2) minor(2) reserved(4) verlen(4)
    version(verlen, 4-byte padded) flags(2) streamcount(2) then one header per
    stream: offset(4) size(4) name(asciiz, 4-byte padded).

2.  Parse the table stream (``#~``, or ``#-`` when the compiler emitted the
    uncompressed/edit-and-continue form).  Header: reserved(4) major(1)
    minor(1) heapSizes(1) reserved(1) valid(8) sorted(8), then a uint32 row
    count for each bit set in ``valid``, then the rows themselves, packed with
    no padding.  Row *sizes* are not stored anywhere: they must be recomputed
    from the schema (II.22), because heap indexes are 2 or 4 bytes depending on
    ``heapSizes``, simple table indexes are 2 or 4 bytes depending on whether
    the target table has 2^16 rows or more, and coded indexes are 2 or 4 bytes
    depending on the largest table in their tag set.  That is why this file
    carries the full schema for all 45 tables -- we must be able to compute the
    size of every table that precedes the ones we care about in order to find
    them at all.

3.  TypeDef (0x02) gives the enum's name/namespace and a ``FieldList`` index.
    That index is the *start* of a run in the Field table (0x04); the run ends
    where the next TypeDef's FieldList starts (the "list column" convention of
    II.22).  NestedClass (0x29) is used to reconstruct ``Outer/Inner`` names
    for nested enums.

4.  Each enum member is a Field row with FieldAttributes Static|Literal
    (0x10|0x40).  (The one non-literal field is the enum's ``value__`` storage
    slot -- skipped.)  The literal value lives in the Constant table (0x0B),
    keyed by a HasConstant coded index whose tag 0 means "Field".
    Constant.Type is an ELEMENT_TYPE byte and Constant.Value is a #Blob index;
    the blob is a compressed length followed by the raw little-endian value.

Modes
-----
  --list-assemblies      list every metadata root found in the file
  --list-types REGEX     list TypeDef names matching a case-insensitive regex
  --dump NAME            dump ``member = value`` lines for an enum/type
  --grep-strings REGEX   grep the #Strings heap (useful when you do not yet
                         know what the type is called)

No third-party dependencies; Python 3.8+.
"""

from __future__ import annotations

import argparse
import re
import struct
import sys
from typing import Dict, List, Optional, Sequence, Tuple

# --------------------------------------------------------------------------
# Column kinds
# --------------------------------------------------------------------------
U1, U2, U4, U8 = "u1", "u2", "u4", "u8"
STR, GUID, BLOB = "str", "guid", "blob"


def tbl(name: str) -> str:
    """A simple index into table `name`."""
    return "t:" + name


def coded(name: str) -> str:
    """A coded index of kind `name` (see CODED_INDEXES)."""
    return "c:" + name


# Coded index tag sets (II.24.2.6).  None = reserved/unused tag.
CODED_INDEXES: Dict[str, Sequence[Optional[str]]] = {
    "TypeDefOrRef": ("TypeDef", "TypeRef", "TypeSpec"),
    "HasConstant": ("Field", "Param", "Property"),
    "HasCustomAttribute": (
        "MethodDef", "Field", "TypeRef", "TypeDef", "Param", "InterfaceImpl",
        "MemberRef", "Module", "DeclSecurity", "Property", "Event",
        "StandAloneSig", "ModuleRef", "TypeSpec", "Assembly", "AssemblyRef",
        "File", "ExportedType", "ManifestResource", "GenericParam",
        "GenericParamConstraint", "MethodSpec",
    ),
    "HasFieldMarshal": ("Field", "Param"),
    "HasDeclSecurity": ("TypeDef", "MethodDef", "Assembly"),
    "MemberRefParent": ("TypeDef", "TypeRef", "ModuleRef", "MethodDef",
                        "TypeSpec"),
    "HasSemantics": ("Event", "Property"),
    "MethodDefOrRef": ("MethodDef", "MemberRef"),
    "MemberForwarded": ("Field", "MethodDef"),
    "Implementation": ("File", "AssemblyRef", "ExportedType"),
    "CustomAttributeType": (None, None, "MethodDef", "MemberRef", None),
    "ResolutionScope": ("Module", "ModuleRef", "AssemblyRef", "TypeRef"),
    "TypeOrMethodDef": ("TypeDef", "MethodDef"),
    # Portable PDB
    "HasCustomDebugInformation": (
        "MethodDef", "Field", "TypeRef", "TypeDef", "Param", "InterfaceImpl",
        "MemberRef", "Module", "DeclSecurity", "Property", "Event",
        "StandAloneSig", "ModuleRef", "TypeSpec", "Assembly", "AssemblyRef",
        "File", "ExportedType", "ManifestResource", "GenericParam",
        "GenericParamConstraint", "MethodSpec", "Document",
        "MethodDebugInformation", "LocalScope", "LocalVariable",
        "LocalConstant", "ImportScope",
    ),
}

# table number -> (name, [(column name, kind), ...])   (II.22)
TABLES: Dict[int, Tuple[str, Sequence[Tuple[str, str]]]] = {
    0x00: ("Module", (("Generation", U2), ("Name", STR), ("Mvid", GUID),
                      ("EncId", GUID), ("EncBaseId", GUID))),
    0x01: ("TypeRef", (("ResolutionScope", coded("ResolutionScope")),
                       ("Name", STR), ("Namespace", STR))),
    0x02: ("TypeDef", (("Flags", U4), ("Name", STR), ("Namespace", STR),
                       ("Extends", coded("TypeDefOrRef")),
                       ("FieldList", tbl("Field")),
                       ("MethodList", tbl("MethodDef")))),
    0x03: ("FieldPtr", (("Field", tbl("Field")),)),
    0x04: ("Field", (("Flags", U2), ("Name", STR), ("Signature", BLOB))),
    0x05: ("MethodPtr", (("Method", tbl("MethodDef")),)),
    0x06: ("MethodDef", (("RVA", U4), ("ImplFlags", U2), ("Flags", U2),
                         ("Name", STR), ("Signature", BLOB),
                         ("ParamList", tbl("Param")))),
    0x07: ("ParamPtr", (("Param", tbl("Param")),)),
    0x08: ("Param", (("Flags", U2), ("Sequence", U2), ("Name", STR))),
    0x09: ("InterfaceImpl", (("Class", tbl("TypeDef")),
                             ("Interface", coded("TypeDefOrRef")))),
    0x0A: ("MemberRef", (("Class", coded("MemberRefParent")), ("Name", STR),
                         ("Signature", BLOB))),
    0x0B: ("Constant", (("Type", U1), ("Padding", U1),
                        ("Parent", coded("HasConstant")), ("Value", BLOB))),
    0x0C: ("CustomAttribute", (("Parent", coded("HasCustomAttribute")),
                               ("Type", coded("CustomAttributeType")),
                               ("Value", BLOB))),
    0x0D: ("FieldMarshal", (("Parent", coded("HasFieldMarshal")),
                            ("NativeType", BLOB))),
    0x0E: ("DeclSecurity", (("Action", U2),
                            ("Parent", coded("HasDeclSecurity")),
                            ("PermissionSet", BLOB))),
    0x0F: ("ClassLayout", (("PackingSize", U2), ("ClassSize", U4),
                           ("Parent", tbl("TypeDef")))),
    0x10: ("FieldLayout", (("Offset", U4), ("Field", tbl("Field")))),
    0x11: ("StandAloneSig", (("Signature", BLOB),)),
    0x12: ("EventMap", (("Parent", tbl("TypeDef")),
                        ("EventList", tbl("Event")))),
    0x13: ("EventPtr", (("Event", tbl("Event")),)),
    0x14: ("Event", (("EventFlags", U2), ("Name", STR),
                     ("EventType", coded("TypeDefOrRef")))),
    0x15: ("PropertyMap", (("Parent", tbl("TypeDef")),
                           ("PropertyList", tbl("Property")))),
    0x16: ("PropertyPtr", (("Property", tbl("Property")),)),
    0x17: ("Property", (("Flags", U2), ("Name", STR), ("Type", BLOB))),
    0x18: ("MethodSemantics", (("Semantics", U2), ("Method", tbl("MethodDef")),
                               ("Association", coded("HasSemantics")))),
    0x19: ("MethodImpl", (("Class", tbl("TypeDef")),
                          ("MethodBody", coded("MethodDefOrRef")),
                          ("MethodDeclaration", coded("MethodDefOrRef")))),
    0x1A: ("ModuleRef", (("Name", STR),)),
    0x1B: ("TypeSpec", (("Signature", BLOB),)),
    0x1C: ("ImplMap", (("MappingFlags", U2),
                       ("MemberForwarded", coded("MemberForwarded")),
                       ("ImportName", STR), ("ImportScope", tbl("ModuleRef")))),
    0x1D: ("FieldRVA", (("RVA", U4), ("Field", tbl("Field")))),
    0x1E: ("EncLog", (("Token", U4), ("FuncCode", U4))),
    0x1F: ("EncMap", (("Token", U4),)),
    0x20: ("Assembly", (("HashAlgId", U4), ("MajorVersion", U2),
                        ("MinorVersion", U2), ("BuildNumber", U2),
                        ("RevisionNumber", U2), ("Flags", U4),
                        ("PublicKey", BLOB), ("Name", STR), ("Culture", STR))),
    0x21: ("AssemblyProcessor", (("Processor", U4),)),
    0x22: ("AssemblyOS", (("OSPlatformID", U4), ("OSMajorVersion", U4),
                          ("OSMinorVersion", U4))),
    0x23: ("AssemblyRef", (("MajorVersion", U2), ("MinorVersion", U2),
                           ("BuildNumber", U2), ("RevisionNumber", U2),
                           ("Flags", U4), ("PublicKeyOrToken", BLOB),
                           ("Name", STR), ("Culture", STR),
                           ("HashValue", BLOB))),
    0x24: ("AssemblyRefProcessor", (("Processor", U4),
                                    ("AssemblyRef", tbl("AssemblyRef")))),
    0x25: ("AssemblyRefOS", (("OSPlatformId", U4), ("OSMajorVersion", U4),
                             ("OSMinorVersion", U4),
                             ("AssemblyRef", tbl("AssemblyRef")))),
    0x26: ("File", (("Flags", U4), ("Name", STR), ("HashValue", BLOB))),
    0x27: ("ExportedType", (("Flags", U4), ("TypeDefId", U4), ("Name", STR),
                            ("Namespace", STR),
                            ("Implementation", coded("Implementation")))),
    0x28: ("ManifestResource", (("Offset", U4), ("Flags", U4), ("Name", STR),
                                ("Implementation", coded("Implementation")))),
    0x29: ("NestedClass", (("NestedClass", tbl("TypeDef")),
                           ("EnclosingClass", tbl("TypeDef")))),
    0x2A: ("GenericParam", (("Number", U2), ("Flags", U2),
                            ("Owner", coded("TypeOrMethodDef")),
                            ("Name", STR))),
    0x2B: ("MethodSpec", (("Method", coded("MethodDefOrRef")),
                          ("Instantiation", BLOB))),
    0x2C: ("GenericParamConstraint", (("Owner", tbl("GenericParam")),
                                      ("Constraint", coded("TypeDefOrRef")))),
    # Portable PDB tables (only present in .pdb metadata; harmless to keep).
    0x30: ("Document", (("Name", BLOB), ("HashAlgorithm", GUID),
                        ("Hash", BLOB), ("Language", GUID))),
    0x31: ("MethodDebugInformation", (("Document", tbl("Document")),
                                      ("SequencePoints", BLOB))),
    0x32: ("LocalScope", (("Method", tbl("MethodDef")),
                          ("ImportScope", tbl("ImportScope")),
                          ("VariableList", tbl("LocalVariable")),
                          ("ConstantList", tbl("LocalConstant")),
                          ("StartOffset", U4), ("Length", U4))),
    0x33: ("LocalVariable", (("Attributes", U2), ("Index", U2),
                             ("Name", STR))),
    0x34: ("LocalConstant", (("Name", STR), ("Signature", BLOB))),
    0x35: ("ImportScope", (("Parent", tbl("ImportScope")), ("Imports", BLOB))),
    0x36: ("StateMachineMethod", (("MoveNextMethod", tbl("MethodDef")),
                                  ("KickoffMethod", tbl("MethodDef")))),
    0x37: ("CustomDebugInformation",
           (("Parent", coded("HasCustomDebugInformation")), ("Kind", GUID),
            ("Value", BLOB))),
}

# FieldAttributes
FIELD_STATIC = 0x0010
FIELD_LITERAL = 0x0040

# ELEMENT_TYPE_* codes usable as Constant.Type -> (struct format, byte width)
_CONST_FMT = {
    0x02: ("<b", 1),   # BOOLEAN
    0x03: ("<H", 2),   # CHAR
    0x04: ("<b", 1),   # I1
    0x05: ("<B", 1),   # U1
    0x06: ("<h", 2),   # I2
    0x07: ("<H", 2),   # U2
    0x08: ("<i", 4),   # I4
    0x09: ("<I", 4),   # U4
    0x0A: ("<q", 8),   # I8
    0x0B: ("<Q", 8),   # U8
    0x0C: ("<f", 4),   # R4
    0x0D: ("<d", 8),   # R8
}


def _heap_widths(heap_sizes: int) -> Tuple[int, int, int]:
    """Byte widths (2 or 4) of #Strings/#GUID/#Blob heap indexes, decoded
    from the table stream header's ``heapSizes`` byte (II.24.2.6): bit 0
    selects wide #Strings indexes, bit 1 wide #GUID indexes, bit 2 wide
    #Blob indexes. Pulled out as its own function so `_self_test` can
    exercise the bit positions directly.
    """
    return (
        4 if heap_sizes & 1 else 2,
        4 if heap_sizes & 2 else 2,
        4 if heap_sizes & 4 else 2,
    )


class MetadataError(Exception):
    pass


class Metadata:
    """One CLI metadata root: heaps plus lazily decoded tables."""

    def __init__(self, buf: bytes, root: int):
        self.buf = buf
        self.root = root
        self.streams: Dict[str, Tuple[int, int]] = {}
        self._read_root()
        self._read_tables()

    # -- metadata root ----------------------------------------------------
    def _read_root(self) -> None:
        b, o = self.buf, self.root
        if b[o:o + 4] != b"BSJB":
            raise MetadataError("no BSJB signature")
        verlen = struct.unpack_from("<I", b, o + 12)[0]
        if not 0 < verlen <= 512:
            raise MetadataError("implausible version length")
        self.version = b[o + 16:o + 16 + verlen].split(b"\0")[0].decode(
            "utf-8", "replace")
        p = o + 16 + ((verlen + 3) & ~3)
        nstreams = struct.unpack_from("<H", b, p + 2)[0]
        p += 4
        if not 1 <= nstreams <= 16:
            raise MetadataError("implausible stream count")
        for _ in range(nstreams):
            off, size = struct.unpack_from("<II", b, p)
            p += 8
            end = b.index(b"\0", p)
            name = b[p:end].decode("ascii", "replace")
            p = end + 1
            p = o + ((p - o + 3) & ~3)   # names are padded to a 4-byte boundary
            self.streams[name] = (o + off, size)

    def _stream(self, *names: str) -> Optional[Tuple[int, int]]:
        for n in names:
            if n in self.streams:
                return self.streams[n]
        return None

    # -- heaps ------------------------------------------------------------
    def string(self, idx: int) -> str:
        base, size = self.streams["#Strings"]
        if idx >= size:
            return ""
        end = self.buf.index(b"\0", base + idx)
        return self.buf[base + idx:end].decode("utf-8", "replace")

    def blob(self, idx: int) -> bytes:
        base, _size = self.streams["#Blob"]
        p = base + idx
        b = self.buf
        first = b[p]
        if first & 0x80 == 0:                      # 1-byte compressed length
            length, p = first & 0x7F, p + 1
        elif first & 0xC0 == 0x80:                 # 2-byte
            length = ((first & 0x3F) << 8) | b[p + 1]
            p += 2
        else:                                      # 4-byte
            length = (((first & 0x1F) << 24) | (b[p + 1] << 16)
                      | (b[p + 2] << 8) | b[p + 3])
            p += 4
        return b[p:p + length]

    # -- table stream -----------------------------------------------------
    def _read_tables(self) -> None:
        st = self._stream("#~", "#-")
        if st is None:
            raise MetadataError("no table stream")
        base, _size = st
        b = self.buf
        heap_sizes = b[base + 6]
        valid, sorted_mask = struct.unpack_from("<QQ", b, base + 8)
        self.sorted_mask = sorted_mask
        p = base + 24
        self.rows: Dict[str, int] = {}
        present = [i for i in range(64) if valid >> i & 1]
        for i in present:
            if i not in TABLES:
                raise MetadataError("unknown table 0x%02X" % i)
            self.rows[TABLES[i][0]] = struct.unpack_from("<I", b, p)[0]
            p += 4
        self.str_w, self.guid_w, self.blob_w = _heap_widths(heap_sizes)

        # Column widths depend on the row counts we just read.
        self._layout: Dict[str, Tuple[int, List[Tuple[str, int, int]]]] = {}
        for i in present:
            name, cols = TABLES[i]
            off = 0
            spec: List[Tuple[str, int, int]] = []
            for cname, kind in cols:
                w = self._width(kind)
                spec.append((cname, off, w))
                off += w
            self._layout[name] = (off, spec)

        # Rows are stored back-to-back in table-number order.
        self._start: Dict[str, int] = {}
        for i in present:
            name = TABLES[i][0]
            self._start[name] = p
            p += self._layout[name][0] * self.rows[name]

    def _width(self, kind: str) -> int:
        if kind == U1:
            return 1
        if kind == U2:
            return 2
        if kind == U4:
            return 4
        if kind == U8:
            return 8
        if kind == STR:
            return self.str_w
        if kind == GUID:
            return self.guid_w
        if kind == BLOB:
            return self.blob_w
        if kind.startswith("t:"):
            return 4 if self.rows.get(kind[2:], 0) >= 0x10000 else 2
        if kind.startswith("c:"):
            tags = CODED_INDEXES[kind[2:]]
            bits = max(1, (len(tags) - 1).bit_length())
            limit = 1 << (16 - bits)
            biggest = max([self.rows.get(t, 0) for t in tags if t] or [0])
            return 4 if biggest >= limit else 2
        raise MetadataError("bad column kind " + kind)

    def cell(self, table: str, row: int, col: str) -> int:
        """One raw column value; `row` is 1-based as tokens are."""
        size, spec = self._layout[table]
        base = self._start[table] + (row - 1) * size
        for cname, off, w in spec:
            if cname == col:
                if w == 2:
                    return struct.unpack_from("<H", self.buf, base + off)[0]
                if w == 4:
                    return struct.unpack_from("<I", self.buf, base + off)[0]
                if w == 1:
                    return self.buf[base + off]
                return struct.unpack_from("<Q", self.buf, base + off)[0]
        raise MetadataError("no column %s.%s" % (table, col))

    def column(self, table: str, col: str) -> List[int]:
        """Every value of one column; list index 0 corresponds to row 1."""
        n = self.rows.get(table, 0)
        if not n:
            return []
        size, spec = self._layout[table]
        base = self._start[table]
        for cname, off, w in spec:
            if cname == col:
                buf = self.buf
                if w == 1:
                    return [buf[base + i * size + off] for i in range(n)]
                unpack = struct.Struct({2: "<H", 4: "<I", 8: "<Q"}[w]).unpack_from
                return [unpack(buf, base + i * size + off)[0]
                        for i in range(n)]
        raise MetadataError("no column %s.%s" % (table, col))

    @staticmethod
    def decode_coded(kind: str, value: int) -> Tuple[Optional[str], int]:
        tags = CODED_INDEXES[kind]
        bits = max(1, (len(tags) - 1).bit_length())
        tag = value & ((1 << bits) - 1)
        return (tags[tag] if tag < len(tags) else None), value >> bits

    # -- higher level -----------------------------------------------------
    def type_names(self) -> List[str]:
        """Full names for every TypeDef; list index 0 == TypeDef row 1."""
        n = self.rows.get("TypeDef", 0)
        names = [self.string(i) for i in self.column("TypeDef", "Name")]
        nss = [self.string(i) for i in self.column("TypeDef", "Namespace")]
        enclosing: Dict[int, int] = {}
        if self.rows.get("NestedClass"):
            enclosing = dict(zip(self.column("NestedClass", "NestedClass"),
                                 self.column("NestedClass", "EnclosingClass")))
        out = []
        for i in range(1, n + 1):
            parts = [names[i - 1]]
            cur, guard = i, 0
            while cur in enclosing and guard < 16:
                cur = enclosing[cur]
                parts.append(names[cur - 1])
                guard += 1
            ns = nss[cur - 1]
            full = "/".join(reversed(parts))
            out.append(ns + "." + full if ns else full)
        return out

    def constants_by_field(self) -> Dict[int, Tuple[int, int]]:
        """Field row -> (ELEMENT_TYPE, blob index)."""
        out: Dict[int, Tuple[int, int]] = {}
        if not self.rows.get("Constant"):
            return out
        for t, par, bl in zip(self.column("Constant", "Type"),
                              self.column("Constant", "Parent"),
                              self.column("Constant", "Value")):
            table, row = self.decode_coded("HasConstant", par)
            if table == "Field":
                out[row] = (t, bl)
        return out

    def field_range(self, typedef_row: int) -> Tuple[int, int]:
        """Half-open Field row range owned by a TypeDef (1-based rows)."""
        n = self.rows["TypeDef"]
        start = self.cell("TypeDef", typedef_row, "FieldList")
        if typedef_row < n:
            end = self.cell("TypeDef", typedef_row + 1, "FieldList")
        else:
            end = self.rows.get("Field", 0) + 1
        return start, end

    def resolve_field(self, row: int) -> int:
        """Apply the FieldPtr indirection table when one is present."""
        if self.rows.get("FieldPtr"):
            return self.cell("FieldPtr", row, "Field")
        return row

    def decode_constant(self, etype: int, blob_idx: int):
        data = self.blob(blob_idx)
        if etype == 0x0E:                      # STRING, UTF-16LE
            return data.decode("utf-16-le", "replace")
        if etype in _CONST_FMT:
            fmt, size = _CONST_FMT[etype]
            if len(data) < size:
                return None
            val = struct.unpack_from(fmt, data)[0]
            return bool(val) if etype == 0x02 else val
        return None


def _fake_metadata(rows: Dict[str, int], str_w: int = 2, guid_w: int = 2,
                    blob_w: int = 2) -> Metadata:
    """A `Metadata` instance with pre-set row counts and heap widths, no
    backing buffer. `_width` only ever reads `.rows`/`.str_w`/`.guid_w`/
    `.blob_w`, so this is enough to exercise its layout math without
    parsing a real assembly.
    """
    md = Metadata.__new__(Metadata)
    md.rows = rows
    md.str_w, md.guid_w, md.blob_w = str_w, guid_w, blob_w
    return md


def _self_test() -> None:
    """Fast synthetic checks for the pure ECMA-335 layout math -- coded and
    simple index widths, table row sizes, and heap index selection -- run
    unconditionally on import. There is no Python test harness elsewhere in
    this repo (see `scripts/gen-name-tables.py`'s `_self_test`), so this
    hard-fails inline instead of living in an untested, never-run test
    file. Fixtures are built in-memory; no real .dll, network, or
    filesystem access is used.
    """
    # -- heap-index width selection (II.24.2.6 heapSizes byte) ------------
    assert _heap_widths(0b000) == (2, 2, 2)
    assert _heap_widths(0b001) == (4, 2, 2)
    assert _heap_widths(0b010) == (2, 4, 2)
    assert _heap_widths(0b100) == (2, 2, 4)
    assert _heap_widths(0b101) == (4, 2, 4)
    assert _heap_widths(0b111) == (4, 4, 4)

    any_md = _fake_metadata({})
    assert any_md._width(U1) == 1
    assert any_md._width(U2) == 2
    assert any_md._width(U4) == 4
    assert any_md._width(U8) == 8
    assert _fake_metadata({}, guid_w=4)._width(GUID) == 4
    try:
        any_md._width("bogus")
    except MetadataError:
        pass
    else:
        raise AssertionError("an unknown column kind must raise MetadataError")

    # -- simple table index width: 2 bytes below 2**16 rows, 4 at/above ---
    assert _fake_metadata({"Field": 0xFFFF})._width(tbl("Field")) == 2
    assert _fake_metadata({"Field": 0x10000})._width(tbl("Field")) == 4
    assert _fake_metadata({})._width(tbl("Field")) == 2, (
        "a table absent from `rows` must be treated as zero rows, not KeyError"
    )

    # -- coded-index width: HasConstant = {Field, Param, Property}, 3 tags
    # -> 2 tag bits (`(3 - 1).bit_length()`) -> widens at 2**(16-2) = 16384
    # rows in the *largest* of the three target tables (II.24.2.6). This is
    # the coded index the Constant table keys on to find an enum member's
    # literal value, so getting its tag order and width math wrong would
    # misread every enum this script dumps.
    assert _fake_metadata(
        {"Field": 16_383, "Param": 0, "Property": 0}
    )._width(coded("HasConstant")) == 2
    assert _fake_metadata(
        {"Field": 16_384, "Param": 0, "Property": 0}
    )._width(coded("HasConstant")) == 4
    # The largest tag's table governs, not the first one listed.
    assert _fake_metadata(
        {"Field": 0, "Param": 0, "Property": 16_384}
    )._width(coded("HasConstant")) == 4

    # A coded index with reserved (None) tags: CustomAttributeType has 5
    # slots (3 real, 2 reserved) -> 3 tag bits -> widens at 2**13 = 8192.
    # Reserved slots must be excluded from the "biggest table" scan.
    assert _fake_metadata(
        {"MethodDef": 8_191, "MemberRef": 0}
    )._width(coded("CustomAttributeType")) == 2
    assert _fake_metadata(
        {"MethodDef": 8_192, "MemberRef": 0}
    )._width(coded("CustomAttributeType")) == 4

    # A wider tag set: HasCustomAttribute has 22 tags -> 5 tag bits ->
    # widens at 2**11 = 2048.
    assert _fake_metadata({"Assembly": 2_047})._width(coded("HasCustomAttribute")) == 2
    assert _fake_metadata({"Assembly": 2_048})._width(coded("HasCustomAttribute")) == 4

    # HasConstant tag order (0=Field, 1=Param, 2=Property) must decode back
    # correctly, and an out-of-range tag (reserved slot 3) must yield `None`
    # rather than a wrong table.
    assert Metadata.decode_coded("HasConstant", (5 << 2) | 0) == ("Field", 5)
    assert Metadata.decode_coded("HasConstant", (7 << 2) | 2) == ("Property", 7)
    assert Metadata.decode_coded("HasConstant", 3) == (None, 0)

    # -- table row-size computation: sum of column widths, in schema order.
    def row_size(table_num: int, rows: Dict[str, int], str_w: int = 2,
                 guid_w: int = 2, blob_w: int = 2) -> int:
        md = _fake_metadata(rows, str_w, guid_w, blob_w)
        _name, cols = TABLES[table_num]
        return sum(md._width(kind) for _cname, kind in cols)

    # Field (0x04): Flags(u2) + Name(str) + Signature(blob).
    assert row_size(0x04, {}) == 2 + 2 + 2
    assert row_size(0x04, {}, str_w=4, blob_w=4) == 2 + 4 + 4

    # Constant (0x0B): Type(u1) + Padding(u1) + Parent(coded HasConstant) +
    # Value(blob) -- the row that carries every enum member's literal.
    assert row_size(0x0B, {"Field": 0, "Param": 0, "Property": 0}) == 1 + 1 + 2 + 2
    assert row_size(
        0x0B, {"Field": 20_000, "Param": 0, "Property": 0}, blob_w=4
    ) == 1 + 1 + 4 + 4

    # TypeDef (0x02): Flags(u4) + Name(str) + Namespace(str) +
    # Extends(coded TypeDefOrRef, 3 tags -> widens at 16384) +
    # FieldList(t:Field) + MethodList(t:MethodDef).
    small_rows = {"TypeDef": 0, "TypeRef": 0, "TypeSpec": 0, "Field": 0, "MethodDef": 0}
    assert row_size(0x02, small_rows) == 4 + 2 + 2 + 2 + 2 + 2
    big_rows = {
        "TypeDef": 0, "TypeRef": 0, "TypeSpec": 16_384,
        "Field": 0x10000, "MethodDef": 0,
    }
    assert row_size(0x02, big_rows) == 4 + 2 + 2 + 4 + 4 + 2


_self_test()


def find_metadata_roots(buf: bytes) -> List[Metadata]:
    """Every parseable CLI metadata root in the file."""
    roots: List[Metadata] = []
    pos = 0
    while True:
        pos = buf.find(b"BSJB", pos)
        if pos < 0:
            break
        try:
            md = Metadata(buf, pos)
            if "#Strings" in md.streams and md.rows.get("TypeDef"):
                roots.append(md)
        except Exception:
            pass
        pos += 4
    return roots


def assembly_name(md: Metadata) -> str:
    if md.rows.get("Assembly"):
        return md.string(md.cell("Assembly", 1, "Name"))
    if md.rows.get("Module"):
        return md.string(md.cell("Module", 1, "Name"))
    return "<anonymous@0x%X>" % md.root


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------
def cmd_list_types(roots: List[Metadata], pattern: str) -> int:
    rx = re.compile(pattern, re.IGNORECASE)
    hits = 0
    for md in roots:
        asm = assembly_name(md)
        for name in md.type_names():
            if rx.search(name):
                print("%s\t%s" % (asm, name))
                hits += 1
    return hits


def cmd_grep_strings(roots: List[Metadata], pattern: str) -> int:
    rx = re.compile(pattern.encode(), re.IGNORECASE)
    hits, seen = 0, set()
    for md in roots:
        base, size = md.streams["#Strings"]
        for s in md.buf[base:base + size].split(b"\0"):
            if s and rx.search(s):
                key = (assembly_name(md), s.decode("utf-8", "replace"))
                if key not in seen:
                    seen.add(key)
                    print("%s\t%s" % key)
                    hits += 1
    return hits


def cmd_dump(roots: List[Metadata], wanted: str, by_value: bool) -> int:
    found = 0
    for md in roots:
        consts: Optional[Dict[int, Tuple[int, int]]] = None
        for i, full in enumerate(md.type_names(), start=1):
            short = full.rsplit(".", 1)[-1]
            if wanted not in (full, short, short.rsplit("/", 1)[-1]):
                continue
            if consts is None:
                consts = md.constants_by_field()
            found += 1
            print("# %s :: %s  (metadata %s)"
                  % (assembly_name(md), full, md.version))
            start, end = md.field_range(i)
            members = []
            for frow in range(start, end):
                real = md.resolve_field(frow)
                flags = md.cell("Field", real, "Flags")
                if not (flags & FIELD_LITERAL and flags & FIELD_STATIC):
                    continue
                if real not in consts:
                    continue
                etype, blob_idx = consts[real]
                members.append((md.string(md.cell("Field", real, "Name")),
                                md.decode_constant(etype, blob_idx)))
            if by_value:
                members.sort(key=lambda m: (m[1] is None, m[1]))
            for name, val in members:
                print("%s = %s" % (name, val))
            print("# %d members" % len(members))
    return found


def main(argv: Optional[Sequence[str]] = None) -> int:
    ap = argparse.ArgumentParser(
        description="Dump .NET enums straight out of ECMA-335 metadata.")
    ap.add_argument("assembly",
                    help="path to a .dll/.exe (single-file bundles OK)")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--dump", metavar="TYPE",
                   help="dump `member = value` for this enum/type name")
    g.add_argument("--list-types", metavar="REGEX",
                   help="list TypeDef names matching a case-insensitive regex")
    g.add_argument("--grep-strings", metavar="REGEX",
                   help="grep the #Strings heap")
    g.add_argument("--list-assemblies", action="store_true",
                   help="list every metadata root found in the file")
    ap.add_argument("--by-value", action="store_true",
                    help="with --dump, sort members by numeric value")
    args = ap.parse_args(argv)

    with open(args.assembly, "rb") as fh:
        buf = fh.read()
    roots = find_metadata_roots(buf)
    if not roots:
        print("no CLI metadata found (compressed bundle?)", file=sys.stderr)
        return 2

    if args.list_assemblies:
        for md in roots:
            print("0x%08X\t%-40s\t%-12s\tTypeDef=%d"
                  % (md.root, assembly_name(md), md.version,
                     md.rows.get("TypeDef", 0)))
        return 0
    if args.list_types:
        return 0 if cmd_list_types(roots, args.list_types) else 1
    if args.grep_strings:
        return 0 if cmd_grep_strings(roots, args.grep_strings) else 1
    if not cmd_dump(roots, args.dump, args.by_value):
        print("type not found: %s" % args.dump, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
