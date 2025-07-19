// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
// UNSAFETY: Contains an impl of GuestMemoryAccess for a test mapping.
#![expect(unsafe_code)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use guestmem::BitmapInfo;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::LockedRange;
use guestmem::ranges::PagedRange;
use smallvec::SmallVec;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU8;
use xtask_fuzz::fuzz_target;

/// An implementation of a GuestMemoryAccess trait that expects all of
/// guest memory to be mapped at a given base with mmap.
/// Pages that are not backed by RAM will return failure
/// when attempting to access them.
struct GuestMemoryMapping {
    mapping: SparseMapping,
    bitmap: Option<Vec<u8>>,
}

// SAFETY: the allocation will stay valid for the lifetime of the object.
unsafe impl GuestMemoryAccess for GuestMemoryMapping {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.mapping.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.mapping.len() as u64
    }

    fn access_bitmap(&self) -> Option<BitmapInfo> {
        self.bitmap.as_ref().map(|bm| BitmapInfo {
            read_bitmap: NonNull::new(bm.as_ptr().cast_mut()).unwrap(),
            write_bitmap: NonNull::new(bm.as_ptr().cast_mut()).unwrap(),
            bit_offset: 0,
        })
    }
}

const PAGE_SIZE: usize = 0x1000;
const SIZE_1MB: usize = 0x100000;
const MAX_PAGES: usize = 8;
const MAX_SIZE: usize = SIZE_1MB * MAX_PAGES;

/// Create a test guest layout:
/// 0           -> 1MB          RAM
/// 1MB         -> 2MB          empty
/// 2MB         -> 3MB          RAM
/// 3MB         -> 3MB + 4K     empty
/// 3MB + 4K    -> 4MB          RAM
fn create_test_mapping() -> GuestMemoryMapping {
    let mapping = SparseMapping::new(SIZE_1MB * 4).unwrap();
    mapping.alloc(0, SIZE_1MB).unwrap();
    mapping.alloc(2 * SIZE_1MB, SIZE_1MB).unwrap();
    mapping
        .alloc(3 * SIZE_1MB + PAGE_SIZE, SIZE_1MB - PAGE_SIZE)
        .unwrap();

    GuestMemoryMapping {
        mapping,
        bitmap: None,
    }
}

fn create_random_mapping(
    bitmap: Vec<u8>,
    allocations: Vec<(usize, usize)>,
) -> std::io::Result<GuestMemoryMapping> {
    let mapping = SparseMapping::new(bitmap.len() * PAGE_SIZE)?;
    for (offset, len) in allocations {
        mapping.alloc(offset % MAX_SIZE, len % MAX_SIZE)?;
    }
    Ok(GuestMemoryMapping {
        mapping,
        bitmap: Some(bitmap),
    })
}

// Implementation of LockedRange for testing
#[repr(C)]
struct AtomicIoVec {
    /// The address of the buffer.
    pub address: *const AtomicU8,
    /// The length of the buffer in bytes.
    pub len: usize,
}

impl From<&'_ [AtomicU8]> for AtomicIoVec {
    fn from(p: &'_ [AtomicU8]) -> Self {
        Self {
            address: p.as_ptr(),
            len: p.len(),
        }
    }
}

struct LockedIoVecs(SmallVec<[AtomicIoVec; 64]>);

impl LockedIoVecs {
    fn new() -> Self {
        Self(Default::default())
    }
}

impl LockedRange for LockedIoVecs {
    fn push_sub_range(&mut self, sub_range: &[AtomicU8]) {
        self.0.push(sub_range.into());
    }
}

#[derive(Arbitrary, Debug)]
enum GuestMemAction {
    Read {
        gpa: u64,
        len: usize,
    },
    ReadPlain {
        gpa: u64,
    },
    ReadAtomic {
        gpa: u64,
        len: usize,
    },
    Write {
        gpa: u64,
        data: Vec<u8>,
    },
    WritePlain {
        gpa: u64,
        data: u128,
    },
    WriteAtomic {
        gpa: u64,
        data: Vec<u8>,
    },
    Fill {
        gpa: u64,
        val: u8,
        len: usize,
    },
    CompareExchange {
        gpa: u64,
        current: u8,
        new: u8,
    },
    Iova {
        gpa: u64,
    },
    LockGpns {
        gpns: Vec<u64>,
    },
    ProbeGpns {
        gpns: Vec<u64>,
    },
    ZeroRange {
        offset: usize,
        len: usize,
        gpns: Vec<u64>,
    },
    ReadRange {
        offset: usize,
        len: usize,
        gpns: Vec<u64>,
    },
    ReadRangeAtomic {
        offset: usize,
        len: usize,
        gpns: Vec<u64>,
    },
    WriteRange {
        offset: usize,
        gpns: Vec<u64>,
        data: Vec<u8>,
    },
    WriteRangeAtomic {
        offset: usize,
        gpns: Vec<u64>,
        data: Vec<u8>,
    },
    LockRange {
        offset: usize,
        len: usize,
        gpns: Vec<u64>,
    },
}

#[derive(Arbitrary, Debug)]
enum MappingOptions {
    Default,
    Random(Vec<u8>, Vec<(usize, usize)>),
}

#[derive(Arbitrary, Debug)]
struct FuzzCase {
    mapping: MappingOptions,
    actions: Vec<GuestMemAction>,
}

fn do_fuzz(input: FuzzCase) {
    let mapping = match input.mapping {
        MappingOptions::Default => create_test_mapping(),
        MappingOptions::Random(bitmap, allocations) => {
            create_random_mapping(bitmap, allocations).unwrap_or(create_test_mapping())
        }
    };
    let gm = GuestMemory::new("fuzz", mapping);
    for action in input.actions {
        match action {
            GuestMemAction::Read { gpa, len } => {
                let len = len % MAX_SIZE;
                let mut data = vec![0u8; len];
                _ = gm.read_at(gpa, &mut data);
            }
            GuestMemAction::ReadPlain { gpa } => {
                _ = gm.read_plain::<u128>(gpa);
            }
            GuestMemAction::ReadAtomic { gpa, len } => {
                let len = len % MAX_SIZE;
                let data: Vec<AtomicU8> = std::iter::repeat_with(|| AtomicU8::new(0))
                    .take(len)
                    .collect();
                _ = gm.read_to_atomic(gpa, &data);
            }
            GuestMemAction::Write { gpa, data } => _ = gm.write_at(gpa, &data),
            GuestMemAction::WritePlain { gpa, data } => _ = gm.write_plain(gpa, &data),
            GuestMemAction::WriteAtomic { gpa, data } => {
                let data_atomic: Vec<AtomicU8> = data.iter().map(|v| AtomicU8::new(*v)).collect();
                _ = gm.write_from_atomic(gpa, &data_atomic);
            }
            GuestMemAction::Fill { gpa, val, len } => _ = gm.fill_at(gpa, val, len),
            GuestMemAction::CompareExchange { gpa, current, new } => {
                _ = gm.compare_exchange(gpa, current, new)
            }
            GuestMemAction::Iova { gpa } => _ = gm.iova(gpa),
            GuestMemAction::LockGpns { gpns } => {
                _ = gm.lock_gpns(true, &gpns);
            }
            GuestMemAction::ProbeGpns { gpns } => {
                _ = gm.probe_gpns(&gpns);
            }
            GuestMemAction::ZeroRange { offset, len, gpns } => {
                let len = len % MAX_SIZE;
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    _ = gm.zero_range(&range);
                }
            }
            GuestMemAction::ReadRange { offset, len, gpns } => {
                let len = len % MAX_SIZE;
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    let mut data = vec![0u8; range.len()];
                    _ = gm.read_range(&range, &mut data);
                }
            }
            GuestMemAction::WriteRange { offset, gpns, data } => {
                let len = data.len();
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    _ = gm.write_range(&range, &data);
                }
            }
            GuestMemAction::ReadRangeAtomic { offset, len, gpns } => {
                let len = len % MAX_SIZE;
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    let data: Vec<AtomicU8> = std::iter::repeat_with(|| AtomicU8::new(0))
                        .take(len)
                        .collect();
                    _ = gm.read_range_to_atomic(&range, &data);
                }
            }
            GuestMemAction::WriteRangeAtomic { offset, gpns, data } => {
                let len = data.len();
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    let data_atomic: Vec<AtomicU8> =
                        data.iter().map(|v| AtomicU8::new(*v)).collect();
                    _ = gm.write_range_from_atomic(&range, &data_atomic);
                }
            }
            GuestMemAction::LockRange { offset, len, gpns } => {
                if let Some(range) = PagedRange::new(offset, len, &gpns) {
                    let locked_range = LockedIoVecs::new();
                    _ = gm.lock_range(range, locked_range);
                }
            }
        }
    }
}

fuzz_target!(|input: FuzzCase| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input);
});
