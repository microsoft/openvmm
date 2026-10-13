// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! Fuzz harness for the VHDX parsing pipeline.
//!
//! Opening a VHDX file requires at least a 1 MiB header area plus valid
//! headers, region tables, metadata, and BAT — far more structure than an
//! unseeded libFuzzer run will synthesize on its own, so a naive harness
//! never gets past the initial size/CRC checks.
//!
//! To reach the parser on every input, the harness first builds a fully
//! valid baseline image with the crate's own [`vhdx::create`] writer (file
//! identifier, dual headers with correct CRC-32C, dual region tables,
//! metadata table, and a zeroed BAT), driven by fuzzer-chosen but
//! valid-bounded geometry. It then lets the fuzzer mutate that image:
//! scattered byte patches corrupt chosen fields, and trailing bytes extend
//! the file. Because the metadata table and BAT carry no checksum, patches
//! there are honored and directly fuzz those parsers; patches in the header
//! or region table exercise the CRC-rejection and dual-copy fallback paths.
//!
//! The mutated image is opened writable with log replay enabled, then a
//! fuzzer-driven sequence of Read/Write/Flush actions runs against it,
//! ending in close, abort, or drop. Writable open spawns a background log
//! task, so the harness runs under [`DefaultPool::run_with`], matching the
//! storvsp and disk_layered fuzzers.

use arbitrary::Arbitrary;
use guid::Guid;
use pal_async::DefaultPool;
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::sync::Arc;
use vhdx::AsyncFile;
use vhdx::CreateParams;
use vhdx::ReadRange;
use vhdx::VhdxFile;
use vhdx::WriteRange;
use vhdx::create;
use xtask_fuzz::fuzz_target;

/// Cap on backing file growth so input-controlled offsets and trailing data
/// cannot cause harness OOMs.
const MAX_FILE_SIZE: usize = 64 << 20;

/// Byte pattern written for the data portion of resolved write ranges.
const WRITE_FILL: u8 = 0xAB;

fn too_large() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::OutOfMemory,
        "fuzz file size limit exceeded",
    )
}

/// In-memory [`AsyncFile`] backing. The data is held behind an `Arc<Mutex>`
/// so the harness keeps a handle after the file is moved into the parser,
/// allowing it to apply the ranges returned by `resolve_write`/`resolve_read`
/// the same way a real caller performs the backing I/O.
#[derive(Clone)]
struct FuzzFile {
    data: Arc<Mutex<Vec<u8>>>,
}

impl FuzzFile {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(Mutex::new(data)),
        }
    }
}

impl AsyncFile for FuzzFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        let data = self.data.lock();
        let off = offset as usize;
        let end = off.saturating_add(buf.len());
        if end > data.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read extends past EOF",
            ));
        }
        buf.copy_from_slice(&data[off..end]);
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let buf = buf.borrow();
        let mut data = self.data.lock();
        let off = offset as usize;
        let end = off.saturating_add(buf.len());
        if end > MAX_FILE_SIZE {
            return Err(too_large());
        }
        if end > data.len() {
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        Ok(self.data.lock().len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let size = usize::try_from(size).map_err(|_| too_large())?;
        if size > MAX_FILE_SIZE {
            return Err(too_large());
        }
        self.data.lock().resize(size, 0);
        Ok(())
    }
}

/// Fuzzer-chosen VHDX geometry, mapped to a valid [`CreateParams`] so the
/// baseline image always builds.
#[derive(Arbitrary, Debug)]
struct ArbParams {
    /// Disk size, expressed as a sector count minus one (so always non-zero
    /// and a multiple of the sector size). Bounded by `u16` to keep the BAT
    /// and backing file small.
    disk_size_sectors_minus_one: u16,
    use_4k_logical: bool,
    use_4k_physical: bool,
    /// Block size selector: `1 << (n % 9)` MiB, i.e. 1..=256 MiB.
    block_size_log2_mb: u8,
    has_parent: bool,
    is_fully_allocated: bool,
    create_incomplete: bool,
    data_write_guid: [u8; 16],
    page_83_data: [u8; 16],
}

impl ArbParams {
    fn to_create_params(&self) -> CreateParams {
        let logical_sector_size = if self.use_4k_logical { 4096 } else { 512 };
        let physical_sector_size = if self.use_4k_physical { 4096 } else { 512 };
        let disk_size =
            logical_sector_size as u64 * (u64::from(self.disk_size_sectors_minus_one) + 1);
        let block_size = (1u32 << (self.block_size_log2_mb % 9)) * 1024 * 1024;

        CreateParams {
            disk_size,
            block_size,
            logical_sector_size,
            physical_sector_size,
            has_parent: self.has_parent,
            block_alignment: 0,
            create_incomplete: self.create_incomplete,
            is_fully_allocated: self.is_fully_allocated,
            data_write_guid: Guid::from_slice(&self.data_write_guid),
            page_83_data: Guid::from_slice(&self.page_83_data),
        }
    }
}

/// A single mutation applied to the baseline image: overwrite up to 8 bytes
/// at `offset`. Offsets past EOF are ignored; this never grows the file.
#[derive(Arbitrary, Debug)]
struct Patch {
    offset: u32,
    bytes: [u8; 8],
}

#[derive(Arbitrary, Debug)]
enum Action {
    Read { sector: u64, count: u16 },
    Write { sector: u64, count: u16 },
    Flush,
}

#[derive(Arbitrary, Debug)]
enum FinalAction {
    Close,
    Abort,
    Drop,
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    params: ArbParams,
    patches: Vec<Patch>,
    actions: Vec<Action>,
    final_action: FinalAction,
    // Last field: the derive hands all remaining bytes to the trailing image
    // data, keeping it independent from the geometry, patches, and actions.
    tail: Vec<u8>,
}

/// Apply the fuzzer's patches to the baseline image, then append the trailing
/// bytes (bounded by `MAX_FILE_SIZE`).
fn mutate_image(data: &mut Vec<u8>, patches: &[Patch], tail: &[u8]) {
    for patch in patches {
        let off = patch.offset as usize;
        if off >= data.len() {
            continue;
        }
        let n = patch.bytes.len().min(data.len() - off);
        data[off..off + n].copy_from_slice(&patch.bytes[..n]);
    }

    if !tail.is_empty() && data.len() < MAX_FILE_SIZE {
        let room = MAX_FILE_SIZE - data.len();
        let take = tail.len().min(room);
        data.extend_from_slice(&tail[..take]);
    }
}

/// Map a fuzzed sector index and count to an aligned, in-bounds byte range so
/// Read/Write actions actually reach BAT resolution instead of bouncing off
/// the alignment/bounds checks.
fn aligned_range(vhdx: &VhdxFile<FuzzFile>, sector: u64, count: u16) -> Option<(u64, u32)> {
    let sector_size = u64::from(vhdx.logical_sector_size());
    let total = vhdx.disk_size() / sector_size;
    if total == 0 {
        return None;
    }
    let start = sector % total;
    let max = (total - start).min(256);
    let n = 1 + u64::from(count) % max;
    Some((start * sector_size, (n * sector_size) as u32))
}

fn do_fuzz(input: FuzzInput) {
    DefaultPool::run_with(async |driver| {
        // Build a valid baseline image with the crate's own writer, bypassing
        // the 1 MiB / CRC / region / metadata sanity checks deterministically.
        let file = FuzzFile::new(Vec::new());
        let backing = file.clone();
        let mut params = input.params.to_create_params();
        if create(&file, &mut params).await.is_err() {
            return;
        }

        // Let the fuzzer corrupt and extend the otherwise-valid image.
        {
            let mut data = backing.data.lock();
            mutate_image(&mut data, &input.patches, &input.tail);
        }

        // Writable open spawns a background log task, so a real executor is
        // required.
        let Ok(vhdx) = VhdxFile::open(file)
            .allow_replay(true)
            .writable(&driver)
            .await
        else {
            return;
        };

        for action in input.actions {
            match action {
                Action::Read { sector, count } => {
                    let Some((offset, len)) = aligned_range(&vhdx, sector, count) else {
                        continue;
                    };
                    let mut ranges = Vec::new();
                    // Hold the guard while performing the backing reads the
                    // resolved ranges describe.
                    if let Ok(_guard) = vhdx.resolve_read(offset, len, &mut ranges).await {
                        for range in &ranges {
                            if let ReadRange::Data {
                                file_offset,
                                length,
                                ..
                            } = *range
                            {
                                let buf = backing.alloc_buffer(length as usize);
                                let _ = backing.read_into(file_offset, buf).await;
                            }
                        }
                    }
                }
                Action::Write { sector, count } => {
                    let Some((offset, len)) = aligned_range(&vhdx, sector, count) else {
                        continue;
                    };
                    let mut ranges = Vec::new();
                    let Ok(guard) = vhdx.resolve_write(offset, len, &mut ranges).await else {
                        continue;
                    };
                    // resolve_write requires every returned range to be written
                    // before complete(); apply them, then finalize. On any
                    // backing write error, drop the guard to abort instead.
                    let mut applied = true;
                    for range in &ranges {
                        let (file_offset, length, fill) = match *range {
                            WriteRange::Data {
                                file_offset,
                                length,
                                ..
                            } => (file_offset, length, WRITE_FILL),
                            WriteRange::Zero {
                                file_offset,
                                length,
                            } => (file_offset, length, 0),
                        };
                        let buf = vec![fill; length as usize];
                        if backing.write_from(file_offset, buf).await.is_err() {
                            applied = false;
                            break;
                        }
                    }
                    if applied {
                        let _ = guard.complete().await;
                    } else {
                        drop(guard);
                    }
                }
                Action::Flush => {
                    let _ = vhdx.flush().await;
                }
            }
        }

        match input.final_action {
            FinalAction::Close => {
                let _ = vhdx.close().await;
            }
            FinalAction::Abort => vhdx.abort().await,
            FinalAction::Drop => drop(vhdx),
        }
    });
}

fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input);
});
