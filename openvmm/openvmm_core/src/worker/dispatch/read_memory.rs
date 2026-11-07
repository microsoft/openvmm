// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest memory reads for [`VmRpc::ReadMemory`] requests.
//!
//! [`VmRpc::ReadMemory`]: openvmm_defs::rpc::VmRpc::ReadMemory

use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::PAGE_SIZE;
use std::collections::TryReserveError;
use thiserror::Error;

const PAGE_SIZE64: u64 = PAGE_SIZE as u64;

/// An error reading guest memory for a [`VmRpc::ReadMemory`] request.
///
/// [`VmRpc::ReadMemory`]: openvmm_defs::rpc::VmRpc::ReadMemory
#[derive(Debug, Error)]
pub(super) enum ReadMemoryError {
    /// The range runs past the end of the address space.
    #[error("{size:#x} bytes at {gpa:#x} run past the end of the address space")]
    Overflow { gpa: u64, size: usize },
    /// Guest memory does not back the whole range.
    #[error("{size:#x} bytes at {gpa:#x} are not all readable guest memory")]
    NotReadable {
        gpa: u64,
        size: usize,
        #[source]
        source: GuestMemoryError,
    },
    /// The host cannot allocate a buffer for the range.
    #[error("failed to allocate {size:#x} bytes for a guest memory read")]
    Allocation {
        size: usize,
        #[source]
        source: TryReserveError,
    },
}

/// Reads the `size` bytes of guest memory at `gpa` for a
/// [`VmRpc::ReadMemory`] request.
///
/// The requester chooses `gpa` and `size`, so no value of either may panic or
/// abort the VM worker. The whole range must be readable before its buffer is
/// allocated: a range that runs past the end of guest memory, or across a hole
/// in it such as an MMIO gap, fails without allocating, so that the buffer is
/// never larger than the guest memory that fills it. The allocation is
/// fallible too, so that a readable range that the host cannot buffer fails
/// the request.
///
/// [`VmRpc::ReadMemory`]: openvmm_defs::rpc::VmRpc::ReadMemory
pub(super) fn read_guest_memory(
    gm: &GuestMemory,
    gpa: u64,
    size: usize,
) -> Result<Vec<u8>, ReadMemoryError> {
    let Some(last_offset) = (size as u64).checked_sub(1) else {
        return Ok(Vec::new());
    };
    let last = gpa
        .checked_add(last_offset)
        .ok_or(ReadMemoryError::Overflow { gpa, size })?;
    let not_readable = |source| ReadMemoryError::NotReadable { gpa, size, source };

    // Guest memory is accessible in whole pages, so a byte of each page of the
    // range shows whether all of it is readable. Probe the last page first, so
    // that a range that runs past the end of guest memory fails without a scan
    // of the memory before it.
    let last_gpn = last / PAGE_SIZE64;
    gm.probe_gpns(&[last_gpn]).map_err(not_readable)?;
    for gpn in gpa / PAGE_SIZE64..last_gpn {
        gm.probe_gpns(&[gpn]).map_err(not_readable)?;
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|source| ReadMemoryError::Allocation { size, source })?;
    bytes.resize(size, 0);
    gm.read_at(gpa, &mut bytes).map_err(not_readable)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use guestmem::AlignedHeapMemory;
    use test_with_tracing::test;

    const RAM_SIZE: usize = 4 * PAGE_SIZE;

    /// Guest RAM of [`RAM_SIZE`] bytes, each holding the low byte of its
    /// address.
    fn ram() -> GuestMemory {
        let gm = GuestMemory::allocate(RAM_SIZE);
        let contents: Vec<u8> = (0..RAM_SIZE).map(|addr| addr as u8).collect();
        gm.write_at(0, &contents).unwrap();
        gm
    }

    /// Reads return the memory of the range, for sizes from zero, which reads
    /// nothing at any address, to the whole of RAM.
    #[test]
    fn reads_ranges_of_ram() {
        let gm = ram();
        let end = RAM_SIZE as u64;
        for (gpa, size) in [
            (0, 0),
            (end, 0),
            (u64::MAX, 0),
            (0, 1),
            (0x123, 1),
            (end - 1, 1),
            (PAGE_SIZE64 - 1, 2),
            (0x123, RAM_SIZE - 0x123),
            (0, RAM_SIZE),
        ] {
            let bytes = read_guest_memory(&gm, gpa, size).unwrap();
            let expected: Vec<u8> = (gpa..gpa.wrapping_add(size as u64))
                .map(|addr| addr as u8)
                .collect();
            assert_eq!(bytes, expected, "{size:#x} bytes at {gpa:#x}");
        }
    }

    /// Ranges that run past the end of RAM fail as unreadable before any
    /// allocation, including ones of sizes that no buffer can have (beyond
    /// `isize::MAX`) or that no host can allocate.
    #[test]
    fn range_past_ram_fails() {
        let gm = ram();
        let end = RAM_SIZE as u64;
        for (gpa, size) in [
            (0, RAM_SIZE + 1),
            (end - 1, 2),
            (end, 1),
            (1 << 62, PAGE_SIZE),
            (u64::MAX, 1),
            (0, isize::MAX as usize),
            (0, usize::MAX),
            (1, usize::MAX),
        ] {
            let err = read_guest_memory(&gm, gpa, size).unwrap_err();
            assert!(
                matches!(err, ReadMemoryError::NotReadable { .. }),
                "{size:#x} bytes at {gpa:#x}: {err:?}"
            );
        }
    }

    /// Ranges whose last byte would lie past the end of the 64-bit address
    /// space fail without wrapping around it.
    #[test]
    fn range_past_address_space_fails() {
        let gm = ram();
        for (gpa, size) in [(2, usize::MAX), (u64::MAX, 2), (u64::MAX - 0xfff, 0x1001)] {
            let err = read_guest_memory(&gm, gpa, size).unwrap_err();
            assert!(
                matches!(err, ReadMemoryError::Overflow { .. }),
                "{size:#x} bytes at {gpa:#x}: {err:?}"
            );
        }
    }

    /// A range across a hole fails before its buffer is allocated, even when
    /// guest memory backs both of its ends.
    #[test]
    fn range_across_hole_fails_before_allocating() {
        // Two regions, each with a page of memory at its start, so that the
        // range from the start of the first to the start of the second is
        // larger than any host can allocate.
        let region_size = 1 << 62;
        let gm = GuestMemory::new_multi_region(
            "sparse",
            region_size,
            vec![
                Some(AlignedHeapMemory::new(PAGE_SIZE)),
                Some(AlignedHeapMemory::new(PAGE_SIZE)),
            ],
        )
        .unwrap();
        let size = usize::try_from(region_size + 1).unwrap();
        assert_eq!(read_guest_memory(&gm, 0, 1).unwrap(), [0]);
        assert_eq!(read_guest_memory(&gm, region_size, 1).unwrap(), [0]);
        let err = read_guest_memory(&gm, 0, size).unwrap_err();
        assert!(
            matches!(err, ReadMemoryError::NotReadable { .. }),
            "{err:?}"
        );
    }
}
