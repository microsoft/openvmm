// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest memory bridge for vhost-user.
//!
//! Builds a [`GuestMemory`] backed by a [`SparseMapping`] from the memory
//! regions received via the vhost-user `SET_MEM_TABLE` message. Each region's
//! fd is mapped at its GPA offset within the sparse mapping, giving the device
//! direct pointer access without per-operation region lookups.
//!
//! Because `GuestMemory` is only provided to the device at queue-start time,
//! a new `GuestMemory` can be constructed on each `SET_MEM_TABLE` without
//! needing dynamically-updateable shared state.

#![cfg(unix)]

use guestmem::GuestMemory;
use sparse_mmap::SparseMapping;
use std::os::fd::OwnedFd;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("region has zero size at GPA {gpa:#x}")]
    ZeroSizeRegion { gpa: u64 },
    #[error("region overflows address space: GPA {gpa:#x} + size {size:#x}")]
    RegionOverflow { gpa: u64, size: u64 },
    #[error("regions overlap: [{gpa_a:#x}..{end_a:#x}) and [{gpa_b:#x}..{end_b:#x})")]
    OverlappingRegions {
        gpa_a: u64,
        end_a: u64,
        gpa_b: u64,
        end_b: u64,
    },
    #[error("failed to reserve VA range for guest memory")]
    Reserve(#[source] std::io::Error),
    #[error("failed to map region at GPA {gpa:#x} (size {size:#x})")]
    MapRegion {
        gpa: u64,
        size: u64,
        #[source]
        source: std::io::Error,
    },
}

/// Parsed region metadata retained for VA→GPA translation.
pub struct MemoryRegionInfo {
    pub guest_phys_addr: u64,
    pub size: u64,
    pub userspace_addr: u64,
}

/// The result of [`build_guest_memory`]: a `GuestMemory` plus the metadata
/// needed for VA→GPA translation of vring addresses.
pub struct VhostUserMemory {
    pub guest_memory: GuestMemory,
    pub regions: Vec<MemoryRegionInfo>,
}

/// Build a new [`GuestMemory`] from a set of vhost-user memory regions.
///
/// Each entry is `(guest_phys_addr, memory_size, userspace_addr, mmap_offset, fd)`.
///
/// The returned `GuestMemory` is backed by a [`SparseMapping`] that covers the
/// entire GPA range up to the highest region end. Each region's fd is mapped
/// at its GPA offset within that reservation, giving direct pointer access.
pub fn build_guest_memory(
    raw_regions: Vec<(u64, u64, u64, u64, OwnedFd)>,
) -> Result<VhostUserMemory, MemoryError> {
    if raw_regions.is_empty() {
        return Ok(VhostUserMemory {
            guest_memory: GuestMemory::empty(),
            regions: Vec::new(),
        });
    }

    // Validate regions: no zero-size, no overflow, no overlaps.
    for &(gpa, size, _, _, _) in &raw_regions {
        if size == 0 {
            return Err(MemoryError::ZeroSizeRegion { gpa });
        }
        gpa.checked_add(size)
            .ok_or(MemoryError::RegionOverflow { gpa, size })?;
    }

    // Check for overlapping regions by sorting on GPA.
    let mut sorted: Vec<(u64, u64)> = raw_regions
        .iter()
        .map(|(gpa, size, _, _, _)| (*gpa, *size))
        .collect();
    sorted.sort_by_key(|(gpa, _)| *gpa);
    for pair in sorted.windows(2) {
        let (gpa_a, size_a) = pair[0];
        let (gpa_b, size_b) = pair[1];
        if gpa_a + size_a > gpa_b {
            return Err(MemoryError::OverlappingRegions {
                gpa_a,
                end_a: gpa_a + size_a,
                gpa_b,
                end_b: gpa_b + size_b,
            });
        }
    }

    // Determine the size of the VA reservation: the end of the highest region.
    let max_addr = raw_regions
        .iter()
        .map(|(gpa, size, _, _, _)| gpa.saturating_add(*size))
        .max()
        .unwrap();

    let mapping = SparseMapping::new(max_addr as usize).map_err(MemoryError::Reserve)?;

    let mut regions = Vec::with_capacity(raw_regions.len());
    for (guest_phys_addr, memory_size, userspace_addr, mmap_offset, fd) in raw_regions {
        mapping
            .map_file(
                guest_phys_addr as usize,
                memory_size as usize,
                fd,
                mmap_offset,
                true, // writable
            )
            .map_err(|e| MemoryError::MapRegion {
                gpa: guest_phys_addr,
                size: memory_size,
                source: e,
            })?;

        regions.push(MemoryRegionInfo {
            guest_phys_addr,
            size: memory_size,
            userspace_addr,
        });
    }

    // SparseMapping implements GuestMemoryAccess (mapping() returns Some(ptr)).
    let guest_memory = GuestMemory::new("vhost-user", mapping);

    Ok(VhostUserMemory {
        guest_memory,
        regions,
    })
}

/// Translate a frontend userspace virtual address to a guest physical address
/// using the region metadata.
pub fn va_to_gpa(regions: &[MemoryRegionInfo], va: u64) -> Option<u64> {
    for r in regions {
        if va >= r.userspace_addr && va < r.userspace_addr.saturating_add(r.size) {
            return Some(va - r.userspace_addr + r.guest_phys_addr);
        }
    }
    None
}
