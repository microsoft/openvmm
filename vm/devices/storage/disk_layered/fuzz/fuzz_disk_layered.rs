// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! Fuzzes sector-range handling in a layered disk with arbitrary
//! `(sector, count)` pairs.
//!
//! Storage frontends pass guest-supplied sector numbers straight through, so a
//! backend has to cope with any `u64` without panicking, wrapping, or operating
//! on the wrong sectors. This is the shape of the bug that motivated the
//! sector-range work: a read near `u64::MAX` overflowed while building the
//! layer's sector bitmap.
//!
//! Because the disk is RAM-backed and never fails for any other reason, the
//! expected outcome of every operation is known exactly: a request lies within
//! the disk, and must succeed, or it does not, and must be rejected with
//! `IllegalBlock`. That is a much stronger oracle than "did not panic" — it
//! also catches an out-of-range request quietly succeeding, which is the more
//! dangerous failure.

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::GuestMemory;
use pal_async::DefaultPool;
use scsi_buffers::OwnedRequestBuffers;
use xtask_fuzz::fuzz_target;

const SECTOR_SIZE: usize = 512;
const DISK_SIZE: u64 = 1024 * 1024;
/// Bounds the buffer so that the fuzzer spends its time on sector numbers
/// rather than on large transfers.
const MAX_SECTORS: usize = 8;

/// Reads and writes always cover at least one sector.
///
/// A zero-length transfer is not interesting here and would break the oracle
/// below: `LayeredDisk` builds an empty bitmap and returns success without ever
/// consulting the range, so a zero-sector read past the end of the disk
/// legitimately succeeds. The conformance suite covers that case directly.
fn transfer_sectors(sectors: u8) -> usize {
    1 + sectors as usize % MAX_SECTORS
}

#[derive(Arbitrary, Debug)]
enum Op {
    Read { sector: u64, sectors: u8 },
    Write { sector: u64, sectors: u8 },
    Unmap { sector: u64, count: u64 },
}

/// Asserts that the result matches what the range implies.
#[track_caller]
fn check(op: &Op, sector: u64, count: u64, sector_count: u64, r: Result<(), DiskError>) {
    let in_range = sector
        .checked_add(count)
        .is_some_and(|end| end <= sector_count);
    match r {
        Ok(()) if in_range => {}
        Err(DiskError::IllegalBlock) if !in_range => {}
        Ok(()) => panic!("{op:?}: out of range but succeeded"),
        Err(err) => panic!("{op:?}: in range but failed: {err:?}"),
    }
}

async fn fuzz_disk(
    disk: Disk,
    mem: &GuestMemory,
    u: &mut Unstructured<'_>,
) -> arbitrary::Result<()> {
    let sector_count = disk.sector_count();
    while !u.is_empty() {
        let op: Op = u.arbitrary()?;
        match op {
            Op::Read { sector, sectors } => {
                let sectors = transfer_sectors(sectors);
                let r = disk
                    .read_vectored(
                        &OwnedRequestBuffers::linear(0, sectors * SECTOR_SIZE, true).buffer(mem),
                        sector,
                    )
                    .await;
                check(&op, sector, sectors as u64, sector_count, r);
            }
            Op::Write { sector, sectors } => {
                let sectors = transfer_sectors(sectors);
                let r = disk
                    .write_vectored(
                        &OwnedRequestBuffers::linear(0, sectors * SECTOR_SIZE, false).buffer(mem),
                        sector,
                        false,
                    )
                    .await;
                check(&op, sector, sectors as u64, sector_count, r);
            }
            Op::Unmap { sector, count } => {
                let r = disk.unmap(sector, count, false).await;
                check(&op, sector, count, sector_count, r);
            }
        }
    }
    Ok(())
}

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    DefaultPool::run_with(async |_driver| {
        let disk = disklayer_ram::ram_disk(DISK_SIZE, false).unwrap();
        let mem = GuestMemory::allocate(MAX_SECTORS * SECTOR_SIZE);
        fuzz_disk(disk, &mem, u).await
    })
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
