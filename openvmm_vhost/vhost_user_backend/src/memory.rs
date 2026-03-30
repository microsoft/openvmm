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

use crate::protocol::VhostUserMemoryRegion;
use guestmem::GuestMemory;
use memory_range::MemoryRange;
use sparse_mmap::SparseMapping;
use std::os::fd::OwnedFd;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("invalid memory region")]
    InvalidRange(#[source] memory_range::InvalidMemoryRange),
    #[error("region overflows address space: GPA {gpa:#x} + size {size:#x}")]
    RegionOverflow { gpa: u64, size: u64 },
    #[error("regions overlap: {a} and {b}")]
    OverlappingRegions { a: MemoryRange, b: MemoryRange },
    #[error("failed to reserve VA range for guest memory")]
    Reserve(#[source] std::io::Error),
    #[error("failed to map region {range}")]
    MapRegion {
        range: MemoryRange,
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
/// The returned `GuestMemory` is backed by a [`SparseMapping`] that covers the
/// entire GPA range up to the highest region end. Each region's fd is mapped
/// at its GPA offset within that reservation, giving direct pointer access.
pub fn build_guest_memory(
    mut raw_regions: Vec<(VhostUserMemoryRegion, OwnedFd)>,
) -> Result<VhostUserMemory, MemoryError> {
    if raw_regions.is_empty() {
        return Ok(VhostUserMemory {
            guest_memory: GuestMemory::empty(),
            regions: Vec::new(),
        });
    }

    // Sort by GPA and validate each region.
    raw_regions.sort_by_key(|(r, _)| r.guest_phys_addr);

    let mut prev: Option<MemoryRange> = None;
    let regions: Vec<MemoryRegionInfo> = raw_regions
        .iter()
        .map(|(region, _)| {
            let gpa = region.guest_phys_addr;
            let end = gpa
                .checked_add(region.memory_size)
                .ok_or(MemoryError::RegionOverflow {
                    gpa,
                    size: region.memory_size,
                })?;
            let range = MemoryRange::try_new(gpa..end).map_err(MemoryError::InvalidRange)?;
            if let Some(prev) = prev {
                if prev.overlaps(&range) {
                    return Err(MemoryError::OverlappingRegions { a: prev, b: range });
                }
            }
            prev = Some(range);
            Ok(MemoryRegionInfo {
                guest_phys_addr: range.start(),
                size: range.len(),
                userspace_addr: region.userspace_addr,
            })
        })
        .collect::<Result<_, _>>()?;

    // The regions are sorted, so the last one has the highest end address.
    let max_addr = regions.last().map(|r| r.guest_phys_addr + r.size).unwrap();
    let mapping = SparseMapping::new(max_addr as usize).map_err(MemoryError::Reserve)?;

    for ((region, fd), info) in raw_regions.iter().zip(&regions) {
        mapping
            .map_file(
                info.guest_phys_addr as usize,
                info.size as usize,
                fd,
                region.mmap_offset,
                true, // writable
            )
            .map_err(|e| MemoryError::MapRegion {
                range: MemoryRange::new(info.guest_phys_addr..info.guest_phys_addr + info.size),
                source: e,
            })?;
    }

    let guest_memory = GuestMemory::new("vhost-user", mapping);

    Ok(VhostUserMemory {
        guest_memory,
        regions,
    })
}

#[derive(Debug, Error)]
#[error("VA {va:#x} not in any SET_MEM_TABLE region")]
pub struct UnmappedVa {
    va: u64,
}

/// Translate a frontend userspace virtual address to a guest physical address
/// using the region metadata.
pub fn va_to_gpa(regions: &[MemoryRegionInfo], va: u64) -> Result<u64, UnmappedVa> {
    for r in regions {
        let offset = va.wrapping_sub(r.userspace_addr);
        if offset < r.size {
            return Ok(r.guest_phys_addr + offset);
        }
    }
    Err(UnmappedVa { va })
}
