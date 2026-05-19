# HyperV Storage File Format

This document describes the on-disk binary format for `.vmrs`, `.vmcx`,
and `.vsv` files used by Hyper-V.

## Overview

The `.vmrs` (and `.vsv`, `.vmcx`) file format used by Hyper-V is built on
**HyperVStorage**, an on-disk key-value store. The format supports typed
values (integers, strings, booleans, binary arrays), hierarchical key
namespaces (XPath-like paths), large binary objects (file objects for RAM
blocks), change tracking, replay logging, and atomic commits.

The same format is used for VM configuration files (`.vmcx`), runtime
state files (`.vmrs`), and saved state files (`.vsv`).

## File Layout

All multi-byte integers are little-endian. Structures are packed (no
padding). Offsets are absolute byte positions from the start of the file.

```
┌─────────────────────────────────────────────┐  offset 0
│  File Header (copy 0)                       │  4096 bytes
├─────────────────────────────────────────────┤  offset 4096
│  File Header (copy 1)                       │  4096 bytes
├─────────────────────────────────────────────┤  offset 8192
│  Root Object Table                          │
├─────────────────────────────────────────────┤
│  Key Tables (allocated sequentially)        │
├─────────────────────────────────────────────┤
│  File Objects (large binary blobs)          │
├─────────────────────────────────────────────┤
│  Additional Object Tables (if needed)       │
├─────────────────────────────────────────────┤
│  Replay Log                                 │
├─────────────────────────────────────────────┤
│  Change Tracking Buffer (optional)          │
└─────────────────────────────────────────────┘
```

Data alignment defaults to 4096 bytes (one page) but can be up to 65536.
All objects in the file are size-aligned to `DataAlignment` boundaries
(i.e., their size is rounded up to a multiple of `DataAlignment`).

Objects are allocated sequentially by bumping a `DataEnd` pointer.
The root object table is always the first object allocated, at offset
8192. Every object (object tables, key tables, file objects) gets an
entry in an object table that records its offset and size.

### How Objects Relate

- **Object tables** are the top-level directory. Each entry points to
  a key table, a file object, or another (overflow) object table.
- **Key tables** store key-value entries inline. Values smaller than 2048
  bytes are stored directly in the key table entry. Large values get a
  **file object**: the key entry stores a `{ SizeInBytes, OffsetInBytes }`
  pointer with the `PointsToFileObject` flag, and the actual data lives
  at that offset.
- **File objects** are raw data blobs at aligned offsets. They have **no
  header** — just raw bytes. Their offset and size are tracked by the
  object table entry that references them.
- The object table knows about all key tables and file objects. When a
  new file object is needed, an empty or free object table entry is
  claimed, the file object data is written at `DataEnd`, and `DataEnd`
  advances.

## File Header

The file header is stored **twice** at fixed offsets: **offset 0** and
**offset 4096**. On load, both copies are read and validated; the one
with the higher sequence number (and valid checksum) is authoritative.
This provides crash consistency — one copy is always valid. The writer
alternates which copy it updates and increments the sequence number.

The first object table starts at offset **8192** (2 × 4096), regardless
of the `DataAlignmentInBytes` field in the header.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | Signature | Must be 0x01282014 |
| 4 | 4 | Checksum | CRC-32 of this structure (with Checksum field zeroed) |
| 8 | 2 | Sequence | Monotonically increasing; used to pick the newer copy |
| 10 | 4 | FormatVersion | See versioning below |
| 14 | 4 | DataVersion | Application-defined data version |
| 18 | 4 | Flags | Reserved |
| 22 | 4 | DataAlignmentInBytes | Alignment for all objects (default 4096) |
| 26 | 8 | ReplayLogOffsetInBytes | Offset to replay log (required) |
| 34 | 8 | ReplayLogSizeInBytes | Size of replay log |
| 42 | 4 | ReplayLogHeaderSizeInBytes | |

Padded to `DataAlignment`.

### Format Versions

Version is encoded as `(major << 8) | minor`:

| Version | Value  | Description |
|---------|--------|-------------|
| 1.0     | 0x0100 | Original, WCHAR key names (no longer supported) |
| 1.1     | 0x0101 | Pre-optimization, non-optimized layout |
| 2.0     | 0x0200 | Optimized layout |
| 3.0     | 0x0300 | Updated key table indexes |
| 3.1     | 0x0301 | Fixed object table entry checksum |
| 4.0     | 0x0400 | Stable array ordering (current) |

Minimum supported version: 1.0. Current version: 4.0.

## Object Tables

Object tables are the top-level directory of the file. They contain
entries that point to other objects (key tables, file objects, or
additional object tables).

### Object Table Header

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | Signature | Must be 0x01110001 |
| 4 | 4 | EntriesCount | Number of entries in this table |

Following the header is an array of `EntriesCount` object table entries.

### Object Table Entry

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 1 | ObjectType | See object type enum |
| 1 | 4 | EntryChecksum | CRC-32 of this entry |
| 5 | 8 | FileOffsetInBytes | Absolute offset of the object in the file |
| 13 | 4 | SizeInBytes | Size of the object |
| 17 | 1 | Flags | 0x01 = required |

### Object Types

| Value | Name | Description |
|-------|------|-------------|
| 0 | Empty | Unused entry |
| 1 | ObjectTable | Points to another object table (for overflow) |
| 2 | KeyTable | Points to a key table |
| 3 | FileObject | Points to a large binary blob |
| 4 | Free | Freed space available for reuse |

The **last entry** in each object table is reserved to point to the
next object table (chaining), or is marked empty if there are no more
tables.

## Key Tables

Key tables store the actual key-value data. Each key table is pointed
to by an object table entry of type `KeyTable` (2).

### Key Table Header

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 2 | Signature | Must be 0x0002 |
| 2 | 2 | TableIndex | Index of this key table |
| 4 | 2 | Sequence | For change detection |
| 6 | 4 | Checksum | CRC-32 of this header struct (with Checksum zeroed) |

Checksum covers **only this 10-byte header**
with the `Checksum` field zeroed — not the table contents.

Following the header is a packed array of key table entries. The usable
space per key table is `table_size - 10` bytes. Default table size is
4096 bytes, giving 4086 usable bytes. Entries are variable-sized; when
a table fills up, a new key table is allocated (each gets its own object
table entry).

The root node is **virtual** — it lives only in memory and is not stored
on disk. Key table indices start at **1** (not 0). Children of the root
use `ParentNodeTable = 0, ParentNodeOffset = 0` as a sentinel meaning
"child of root." The root node's in-memory representation has
`Type = Node`, `NameSizeInSymbols = 0`, and 12 bytes of `NodeData`
(`{ uint64_t ChangeTrackingSequence; uint32_t NextInsertionSequence; }`),
but these fields are reconstructed at load time, not read from disk.

**Node `NextInsertionSequence`**: Each node's `NextInsertionSequence`
must equal `max(child InsertionSequence) + 1`. When adding a child,
`InsertionSequence == 0` is treated as "uninitialized" and triggers
reassignment, which marks the data as changed and causes a failed commit
on read-only files. Therefore, **insertion sequences must be 1-based**
(first child gets 1).

**Key table filling**: Entries must **exactly fill** the key table's data
area (from the end of the header to the end of the object). Any remaining
space must be **> 21 bytes** (strictly greater than
`sizeof(KeyTableEntryHeader)`). The verify loop uses
`while (offset + sizeof(header) < dataEnd)` — strict less-than — so a
Free entry starting at exactly `dataEnd - 21` is unreachable. If the
remaining space after the last entry is ≤ 21 bytes, the entry must be
moved to the next key table.

### Key Table Entry Header

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 1 | Type | Key type (see below) |
| 1 | 1 | Flags | See flags below |
| 2 | 4 | SizeInBytes | Total size of this entry (header + name + data) |
| 6 | 2 | ParentNodeTable | Key table index of parent node |
| 8 | 4 | ParentNodeOffset | Offset within parent's key table |
| 12 | 4 | Checksum | CRC-32 of header (with Checksum zeroed) + name + data |
| 16 | 4 | InsertionSequence | Order preservation for version 4.0+ |
| 20 | 1 | NameSizeInSymbols | Length of the key name in bytes (includes trailing NUL) |

Checksum covers the **21-byte header (with
Checksum zeroed) plus the name and data bytes** — i.e., the meaningful
content, not any slack space at the end of the entry. The checksum field
is at byte offset 12 within the header.

Following the header is:
1. **Name**: `NameSizeInSymbols` bytes of NUL-terminated UTF-8 key name
   (the byte count **includes** the trailing NUL)
2. **Data**: Type-specific data (see below)

### Key Types — Inline Data Layout

Fixed-size types store their value directly after the name with **no
length prefix**:

| Value | Name | Inline Data |
|-------|------|-------------|
| 1 | Free | Entry is unused |
| 3 | Int | `int64_t` (8 bytes, no prefix) |
| 4 | UInt | `uint64_t` (8 bytes, no prefix) |
| 5 | Double | `double` (8 bytes, no prefix) |
| 8 | Bool | `BOOL` (4 bytes / `int32_t`, 0 or 1, no prefix) |
| 9 | Node | `{ uint64_t ChangeTrackingSequence; uint32_t NextInsertionSequence; }` (12 bytes, no prefix) |

Variable-size types have a **`uint32_t SizeInBytes` length prefix**:

| Value | Name | Inline Data |
|-------|------|-------------|
| 6 | String | `{ uint32_t SizeInBytes; wchar_t StringData[]; }` |
| 7 | Array | `{ uint32_t SizeInBytes; uint8_t Data[]; }` |

When the `PointsToFileObject` flag is set (for values >= 2048 bytes),
the data section is **always** replaced with a `FileObjectData` pointer
regardless of type:

### Key Entry Flags

| Bit | Name | Description |
|-----|------|-------------|
| 0x01 | PointsToFileObject | Data is a `FileObjectData` pointer, not inline |
| 0x02 | Subcomponent | Entry is a subcomponent |
| 0x04 | SequenceChangeTracking | Sequence tracking enabled for this node |

### File Object Data (for large values)

When `PointsToFileObject` flag is set, the data section contains:

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | SizeInBytes | Actual data size |
| 4 | 8 | OffsetInBytes | File offset to the file object |

Values of 2048 bytes or larger are stored as file objects rather than
inline in the key table. The threshold is 2048 bytes and the comparison
is `>=` (values exactly 2048 bytes are stored as file objects).

## File Objects

File objects store large binary values that don't fit inline in key
table entries. The threshold is 2048 bytes — values of this size or
larger are stored as file objects (`>=`, not `>`).

A file object is:
1. **Tracked** by an object table entry (type 3 = `FileObject`) that
   records its offset and aligned size
2. **Referenced** by a key table entry with the `PointsToFileObject`
   flag, whose data section contains `{ uint32_t SizeInBytes;
   uint64_t OffsetInBytes; }` pointing to the file object
3. **Stored** as raw data at the recorded offset — no header, no
   framing, just the bytes

When writing a file object:
1. Claim an empty entry in the object table
2. Set it to type `FileObject` (3), with offset = current `DataEnd`,
   size = `AlignedSize(data_length)`
3. Write the raw data at `DataEnd`
4. Advance `DataEnd` by the aligned size
5. Set the key table entry's data to `{ data_length, DataEnd_before }`
   with the `PointsToFileObject` flag

## Key Namespace

Keys are organized hierarchically using paths separated by forward
slashes. For example:

```
/savedstate/savedVM/partition_state
/savedstate/RamMemoryBlock0
/savedstate/RamBlock0
```

The root node is virtual (not stored on disk). Key table indices start
at 1. Children of root have `ParentNodeTable = 0, ParentNodeOffset = 0`.
All other entries have a `ParentNodeTable` and `ParentNodeOffset`
pointing to their parent node entry. Node entries in the tree act as
directories; leaf entries hold typed values.

## Replay Log

The replay log provides atomic commit semantics. Before modifying the
file, changes are first written to the replay log region. On commit,
the changes are applied to the main data area. If the process crashes
mid-commit, the replay log is replayed on the next open.

Even for write-once files, a valid replay log region is **required** —
the loader dereferences the replay log header buffer and verifies
`MaximumNumberOfEntries > 0`. Write an empty replay log (zero current
entries, `MaximumNumberOfEntries` computed from the header size) with
a valid signature and checksum. The header offset and size are stored
in the file header's `ReplayLogOffsetInBytes` and
`ReplayLogSizeInBytes` fields.

## Checksum Algorithm

All checksums use **CRC-32 (ISO 3309 / RFC-1662 / RFC-1952)** — the
standard polynomial 0xEDB88320 reflected form. **Not** CRC-32C. The
checksum field is zeroed before computing the CRC over the structure.

## Appendix A: Saved State Key Schema

For VM saved state (`.vmrs`) files, the key hierarchy under the
`/savedstate` prefix contains:

### VM Version

The VM version is a `uint32_t` encoded as `(major << 8) | minor`:

| Value | Version |
|-------|---------|
| 0x0500 | 5.0 |
| 0x0900 | 9.0 |
| 0x0A00 | 10.0 |
| 0x0C03 | 12.3 (current) |

For a debug dump writer, use a version >= 0x0500 to ensure the
`/savedstate` key prefix is used. 0x0A00 is a safe choice — it's
modern enough for all features but old enough to be widely supported.

### VM Metadata

- `/savedstate/VmVersion` — VM version as **signed integer** (`Int` type,
  NOT `UInt`). E.g., 0x0A00 for v10.0. **Required.**
- `/configuration/properties/version` — integer, same value as
  `VmVersion`. **Required** by some reader versions.
- `/savedstate/type` — saved state type as **string**: `"Normal"`,
  `"Snapshot"`, `"Fast"`, or `"FastWithHandleTransfer"`. Optional.

### Key Path Versioning

Key paths have a version-dependent prefix. For VMs with version >
0x0500, all keys are prefixed with `/savedstate`:

| Component | Old (≤ v5.0) | New (> v5.0) |
|-----------|-----------------|-----------------|
| Partition state | `/savedVM/partition_state` | `/savedstate/savedVM/partition_state` |
| RAM blocks | `RamMemoryBlock/%d/` | `/savedstate/RamMemoryBlock%d` |
| RAM data | `RamBlock%I64u` | `/savedstate/RamBlock%I64u` |

### Processor State

The VP register state is saved as a single binary blob under the
partition state key (`/savedstate/savedVM/partition_state`). This blob
contains a chunked stream of processor register data including general
purpose registers, control registers, segment registers, and other
architectural state for each virtual processor.

### Memory Layout

Guest RAM is stored as **1 MiB blocks** (256 × 4096-byte pages =
1,048,576 bytes).

Two sets of keys are used per memory block:

**Metadata keys** — `RamMemoryBlock%d` (e.g., `RamMemoryBlock0`,
`RamMemoryBlock1`, ...) — stored as Array values containing a
memory block metadata structure:

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | SavedStateVersion | Use version 3 |
| 4 | 4 | Flags | Bitfield: bit 0 = IsHotAdded, bit 1 = IsSgx, bit 2 = IsVtl2Mb, bit 3 = IsSpecificPurpose, bits 4–31 reserved |
| 8 | 8 | PageCountTotal | Number of 4K pages in this block |
| 16 | 8 | MbpIndexStart | Starting MBP (memory block page) index |
| 24 | 8 | GpaIndexStart | Starting GPA page number (GPA / 4096) |
| 32 | 4 | VirtualNode | NUMA node index |
| 36 | 4 | (padding) | Reserved, must be zero |
| 40 | 8 | KsrBlockId | KSR block ID (can be zero for debug dumps) |

Total size: 48 bytes.

For a simple debug dump, set `SavedStateVersion` = 3,
`Flags` = 0, `MbpIndexStart` = cumulative page offset into the RAM
block sequence, `GpaIndexStart` = GPA base / 4096, `VirtualNode` = 0,
`KsrBlockId` = 0.

**Data keys** — `RamBlock%I64u` (e.g., `RamBlock0`, `RamBlock1`, ...) —
stored as Array values containing the raw guest memory data for that
block, up to 1,048,576 bytes.

- If value size == 1,048,576: data is **uncompressed**
- If value size < 1,048,576: data is **XPRESS compressed**

For a debug dump writer, writing uncompressed blocks is simplest.

**GPA mapping**: There is no direct GPA→file-offset table. The reader
enumerates `RamMemoryBlock0`, `RamMemoryBlock1`, ... until a key is not
found, building the GPA map from `GpaIndexStart` and
`PageCountTotal` in each metadata struct. The corresponding RAM data
is at `RamBlock0`, `RamBlock1`, ... in the same order.

**Block numbering**: Metadata blocks (`RamMemoryBlock%d`) and data
blocks (`RamBlock%I64u`) are numbered independently starting from 0.
They are **not** the same count: metadata blocks describe contiguous
GPA ranges (a VM with a 3.5G/0.5G MMIO split has 2 metadata blocks),
while data blocks enumerate every 1 MiB chunk of saved RAM (a 4 GB
VM has ~4096 data blocks). The metadata block's `MbpIndexStart` and
`PageCountTotal` fields map GPA ranges to data block numbers.

Note: metadata keys use `%d` (32-bit format), data keys use `%I64u`
(64-bit format).

### Device State (optional)

- VMBus device state under `/savedstate/configuration/...`
- Skipped when `GuestDebugState` flag is used

### Minimum Required Keys

The following keys must exist for the reader to open the file:

1. `/savedstate/VmVersion` — integer
2. `/configuration/properties/version` — integer, same value
3. `/savedstate/savedVM/partition_state` — array (or file object)
4. `/savedstate/RamMemoryBlock0` — array (at least one metadata entry)

VP count and architecture are derived from the partition state blob,
not from separate keys.

### GUEST_OS_INFO

The partition state blob contains guest OS identification as a 64-bit
value, identical to `HvGuestOsId` in hvdef (and `HV_X64_MSR_GUEST_OS_ID`).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 8 | AsUINT64 | **Closed-source layout** (bit 63 = 0): bits 0–15 BuildNumber, bits 16–23 ServiceVersion, bits 24–31 MinorVersion, bits 32–39 MajorVersion, bits 40–47 OsId (4 = Windows NT), bits 48–62 VendorId (1 = Microsoft), bit 63 IsOpenSource (0). **Open-source layout** (bit 63 = 1): bits 0–31 Version ((major<<16)\|(minor<<8)\|patch), bits 32–39 VendorSpecific2, bits 40–46 OsId (1 = Linux), bit 63 IsOpenSource (1). |

For unenlightened guests, set to 0 (WinDbg will show "Unknown OS" but
still function).
