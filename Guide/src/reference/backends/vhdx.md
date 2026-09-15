# VHDX parser

The `vhdx` crate (`vm/devices/storage/vhdx/`) is a pure-Rust
implementation of the
[VHDX format specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-vhdx/).
It supports dynamic, fixed, and differencing VHDX virtual hard disk
files on all platforms — no Windows APIs or kernel drivers required.

## Features

- **Create** and **open** VHDX files (read-only or writable)
- Dynamic block allocation with four-priority free space management
- Write-ahead log (WAL) for crash-consistent metadata updates
- Sector bitmap tracking for partially-present (differencing) blocks
- Block trim/unmap with multiple modes (file space, free space, zero,
  transparent, soft-anchor removal)
- Concurrent flush coalescing
- Parent locator parsing for differencing disk chains

## Architecture

A VHDX file stores a virtual disk as a collection of fixed-size data
blocks (default 2 MiB) tracked by a Block Allocation Table (BAT).
The crate's write path uses a three-stage pipeline for crash
consistency:

```text
┌───────────┐   commit   ┌──────────┐   apply   ┌────────────┐
│   Cache   │ ──────────►│ Log Task │ ─────────►│ Apply Task │
│ (dirty    │  dirty     │ (WAL     │  logged   │ (final     │
│  pages)   │  pages     │  writer) │  pages    │  offsets)  │
└───────────┘            └──────────┘           └────────────┘
```

1. The **cache** accumulates dirty 4 KiB metadata pages (BAT entries,
   sector bitmap bits). When the dirty count reaches a threshold or
   `flush()` is called, pages are committed to the log task.
2. The **log task** writes WAL entries to the circular log region in
   the VHDX file. On crash, `replay_log()` restores metadata from
   the WAL.
3. The **apply task** writes logged pages to their final file offsets.

Backpressure is managed by a permit semaphore that limits in-flight
pages. A flush sequencer coalesces concurrent flush requests so at
most one file flush is in progress at a time.

## Lifecycle

```rust,ignore
// Create a new empty VHDX file.
create::create(&file, &mut params).await?;

// Open for writing.
let vhdx = VhdxFile::open(file)
    .block_alignment(2 * 1024 * 1024)
    .writable(&spawner)
    .await?;

// Resolve a read — returns file-level ranges.
let mut ranges = Vec::new();
let guard = vhdx.resolve_read(offset, len, &mut ranges).await?;
// ... perform file I/O at the returned offsets ...
drop(guard);

// Resolve a write — returns file-level ranges + I/O guard.
let mut ranges = Vec::new();
let guard = vhdx.resolve_write(offset, len, &mut ranges).await?;
// ... write data at the returned offsets ...
guard.complete().await?;

// Flush to stable storage.
vhdx.flush().await?;

// Clean close (clears log GUID).
vhdx.close().await?;
```

## I/O model

The crate separates **metadata I/O** from **payload I/O**.

Metadata I/O (headers, BAT pages, sector bitmaps, WAL entries) is
handled internally through the `AsyncFile` trait — the caller provides
an `AsyncFile` implementation at open time and never thinks about
metadata again.

Payload I/O (guest data reads and writes) is the caller's
responsibility. `resolve_read()` and `resolve_write()` translate
virtual disk offsets into file-level byte ranges (`ReadRange` /
`WriteRange`). The caller performs its own data I/O at those offsets
using whatever mechanism it prefers (io_uring, standard file I/O,
etc.), then finalizes metadata via the returned I/O guard. This
separation lets the caller use a different, potentially more
performant I/O path for bulk data without the crate imposing any
particular strategy.

- The `vhdx` crate provides the low-level VHDX format implementation
  and I/O resolution API. For OpenVMM integration, the `disklayer_vhdx`
  crate supplies a `LayerIo`-compatible backend used in the layered
  disk storage pipeline.
- For differencing disks, the `vhdx` crate parses parent locator
  metadata, while `disklayer_vhdx::chain::open_vhdx_chain` walks and
  opens parent chains automatically.

## Parent lookup

The Rust chain opener and `vhdxtool check` interpret parent metadata through
`ParentLocator::vhdx_parent()`. This validates the locator type and requires a
nonzero parent linkage GUID. Both verify that the selected parent's data-write
GUID matches the child's recorded linkage.

Both callers use `VhdxParent::candidate_paths()` to enumerate paths. On Windows,
the order is relative path, volume GUID path, then absolute Win32 path, as
specified by MS-VHDX section 2.6.2.6.3. On Unix, only the relative locator is
used, with backslashes translated to native separators. Windows-only locators
are ignored even if their strings happen to name existing Unix files.

`VhdxParent` stores locator strings without host-specific path validation or
normalization. These are VHDX locator strings, not native filesystem paths;
conversion to native paths happens during candidate enumeration. Path syntax
is not validated by these helpers.

Candidate enumeration does not open files. The callers select an existing
candidate before verifying linkage; they do not retry later candidates on a
linkage mismatch or repair stale locators. OpenVMM's normal Windows `.vhdx`
path uses native VHDMP instead of this Rust chain opener.

## Related pages

- [Storage backends](./storage.md) — catalog of all storage backends
- [Storage pipeline](../architecture/devices/storage.md) — how
  frontends, backends, and layers connect
