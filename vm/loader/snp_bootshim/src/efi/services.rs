// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The logic of the boot services, apart from the UEFI calling convention.
//!
//! The guest-only `entry` module reads and writes the caller's pointers, and
//! calls these functions for everything else.

use super::tables::InitrdDevicePath;
use super::tables::VENDOR_NODE_SIZE;
use crate::memory_map::AllocateType;
use crate::memory_map::Error;
use crate::memory_map::MemoryMap;
use guid::Guid;
use memory_range::MemoryRange;
use uefi_specs::uefi::boot::DEVICE_PATH_PROTOCOL_GUID;
use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
use uefi_specs::uefi::boot::LOAD_FILE2_PROTOCOL_GUID;
use uefi_specs::uefi::boot::LOADED_IMAGE_PROTOCOL_GUID;
use uefi_specs::uefi::common::EfiStatus;

/// A pool allocation starts with a header that holds its page count and
/// [`POOL_MAGIC`].
pub(super) const POOL_HEADER_SIZE: u64 = 16;
/// Marks a pool header, so that `FreePool` rejects other pointers.
const POOL_MAGIC: u64 = u64::from_le_bytes(*b"SHIMPOOL");

/// The state of the boot services.
pub(super) struct State {
    pub map: MemoryMap,
    initrd: Option<MemoryRange>,
    initrd_size: u64,
    exited: bool,
    monotonic_count: u64,
}

/// What `LoadFile` must do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LoadFile {
    NotFound,
    /// The buffer is missing or smaller than the initrd.
    TooSmall {
        size: usize,
    },
    /// Copy `size` bytes from `source`.
    Copy {
        source: u64,
        size: usize,
    },
}

/// A protocol interface that the shim provides.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Interface {
    LoadedImage,
    LoadFile2,
    InitrdDevicePath,
}

/// The handles that the shim provides.
pub(super) struct Handles {
    /// The kernel image, which has `LOADED_IMAGE`.
    pub image: u64,
    /// The initrd, which has `LoadFile2` and its device path.
    pub initrd: u64,
}

/// Returns the EFI status for a memory-map error, or `None` if acceptance
/// failed, which stops the guest.
pub(super) fn status(error: Error) -> Option<EfiStatus> {
    match error {
        Error::OutOfResources => Some(EfiStatus::OUT_OF_RESOURCES),
        Error::InvalidParameter => Some(EfiStatus::INVALID_PARAMETER),
        Error::NotFound => Some(EfiStatus::NOT_FOUND),
        Error::Accept => None,
    }
}

/// Decodes the `Type` argument of `AllocatePages` (UEFI spec 7.2). `address`
/// reads the caller's address, which is only an input for some types.
pub(super) fn allocate_type(
    allocate_type: u32,
    address: impl FnOnce() -> u64,
) -> Result<AllocateType, Error> {
    match allocate_type {
        0 => Ok(AllocateType::Any),
        1 => Ok(AllocateType::Max(address())),
        2 => Ok(AllocateType::Address(address())),
        _ => Err(Error::InvalidParameter),
    }
}

/// Returns the pages for a pool allocation of `size` bytes and its header.
pub(super) fn pool_pages(size: usize) -> Option<u64> {
    (size as u64)
        .checked_add(POOL_HEADER_SIZE)
        .map(|size| size.div_ceil(EFI_PAGE_SIZE))
}

/// Returns the header for a pool allocation of `pages` pages.
pub(super) fn pool_header(pages: u64) -> [u64; 2] {
    [pages, POOL_MAGIC]
}

/// Returns the page count from a pool header, or `None` if `header` is not
/// a pool header.
pub(super) fn pool_header_pages(header: [u64; 2]) -> Option<u64> {
    let [pages, magic] = header;
    (magic == POOL_MAGIC).then_some(pages)
}

/// Returns the pool header address for a pool buffer, if the buffer can be
/// one.
pub(super) fn pool_header_address(buffer: u64) -> Option<u64> {
    let address = buffer.checked_sub(POOL_HEADER_SIZE)?;
    address.is_multiple_of(EFI_PAGE_SIZE).then_some(address)
}

/// Returns `true` if `node` is the initrd's vendor media node.
pub(super) fn is_initrd_node(node: &[u8; VENDOR_NODE_SIZE]) -> bool {
    *node == InitrdDevicePath::new().vendor
}

/// Returns the interface for `protocol` on `handle`.
pub(super) fn find_interface(
    handles: &Handles,
    has_initrd: bool,
    handle: u64,
    protocol: Guid,
) -> Option<Interface> {
    if handle == handles.image && protocol == LOADED_IMAGE_PROTOCOL_GUID {
        Some(Interface::LoadedImage)
    } else if handle == handles.initrd && has_initrd {
        if protocol == LOAD_FILE2_PROTOCOL_GUID {
            Some(Interface::LoadFile2)
        } else if protocol == DEVICE_PATH_PROTOCOL_GUID {
            Some(Interface::InitrdDevicePath)
        } else {
            None
        }
    } else {
        None
    }
}

impl State {
    pub fn new(map: MemoryMap, initrd: Option<MemoryRange>, initrd_size: u64) -> Self {
        Self {
            map,
            initrd,
            initrd_size,
            exited: false,
            monotonic_count: 0,
        }
    }

    /// Returns `true` once boot services have ended.
    pub fn exited(&self) -> bool {
        self.exited
    }

    pub fn has_initrd(&self) -> bool {
        self.initrd.is_some()
    }

    /// Ends boot services if `map_key` is the current memory-map key.
    pub fn exit_boot_services(&mut self, map_key: u64) -> EfiStatus {
        if self.exited {
            EfiStatus::UNSUPPORTED
        } else if map_key != self.map.key() {
            EfiStatus::INVALID_PARAMETER
        } else {
            self.exited = true;
            EfiStatus::SUCCESS
        }
    }

    pub fn next_monotonic_count(&mut self) -> u64 {
        self.monotonic_count += 1;
        self.monotonic_count
    }

    /// Decides what `LoadFile` does for a buffer of `buffer_size` bytes, or
    /// for no buffer.
    pub fn load_file(&self, buffer_size: Option<usize>) -> LoadFile {
        let Some(initrd) = self.initrd else {
            return LoadFile::NotFound;
        };
        let size = self.initrd_size as usize;
        match buffer_size {
            Some(available) if available >= size => LoadFile::Copy {
                source: initrd.start(),
                size,
            },
            _ => LoadFile::TooSmall { size },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use uefi_specs::uefi::boot::EfiMemoryType;

    const MB: u64 = 1 << 20;

    fn state(initrd: bool) -> State {
        let mut map = MemoryMap::default();
        map.set(
            MemoryRange::new(0..64 * MB),
            EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
        )
        .unwrap();
        State::new(
            map,
            initrd.then(|| MemoryRange::new(8 * MB..9 * MB)),
            0x1234,
        )
    }

    #[test]
    fn decodes_allocate_types() {
        // `AllocateAnyPages` must not read the address.
        assert_eq!(
            allocate_type(0, || panic!("read the address")),
            Ok(AllocateType::Any)
        );
        assert_eq!(allocate_type(1, || 5), Ok(AllocateType::Max(5)));
        assert_eq!(allocate_type(2, || 6), Ok(AllocateType::Address(6)));
        assert_eq!(
            allocate_type(3, || panic!("read the address")),
            Err(Error::InvalidParameter)
        );
    }

    #[test]
    fn maps_errors_to_statuses() {
        assert_eq!(
            status(Error::OutOfResources),
            Some(EfiStatus::OUT_OF_RESOURCES)
        );
        assert_eq!(
            status(Error::InvalidParameter),
            Some(EfiStatus::INVALID_PARAMETER)
        );
        assert_eq!(status(Error::NotFound), Some(EfiStatus::NOT_FOUND));
        assert_eq!(status(Error::Accept), None);
    }

    #[test]
    fn sizes_pool_allocations() {
        assert_eq!(pool_pages(0), Some(1));
        assert_eq!(pool_pages(4096 - 16), Some(1));
        assert_eq!(pool_pages(4096 - 15), Some(2));
        assert_eq!(pool_pages(usize::MAX), None);
        assert_eq!(pool_header_pages(pool_header(3)), Some(3));
        assert_eq!(pool_header_pages([3, 0]), None);
        assert_eq!(pool_header_address(0x1010), Some(0x1000));
        assert_eq!(pool_header_address(0x1018), None);
        assert_eq!(pool_header_address(8), None);
    }

    #[test]
    fn finds_interfaces() {
        let handles = Handles {
            image: 0x100,
            initrd: 0x200,
        };
        assert_eq!(
            find_interface(&handles, true, 0x100, LOADED_IMAGE_PROTOCOL_GUID),
            Some(Interface::LoadedImage)
        );
        assert_eq!(
            find_interface(&handles, true, 0x200, LOAD_FILE2_PROTOCOL_GUID),
            Some(Interface::LoadFile2)
        );
        assert_eq!(
            find_interface(&handles, true, 0x200, DEVICE_PATH_PROTOCOL_GUID),
            Some(Interface::InitrdDevicePath)
        );
        assert_eq!(
            find_interface(&handles, false, 0x200, LOAD_FILE2_PROTOCOL_GUID),
            None
        );
        assert_eq!(
            find_interface(&handles, true, 0x200, LOADED_IMAGE_PROTOCOL_GUID),
            None
        );
        assert_eq!(
            find_interface(&handles, true, 0x100, LOAD_FILE2_PROTOCOL_GUID),
            None
        );
    }

    #[test]
    fn matches_only_the_initrd_node() {
        let mut node = InitrdDevicePath::new().vendor;
        assert!(is_initrd_node(&node));
        node[VENDOR_NODE_SIZE - 1] ^= 1;
        assert!(!is_initrd_node(&node));
    }

    #[test]
    fn exit_boot_services_checks_the_map_key() {
        let mut state = state(false);
        let key = state.map.key();
        assert_eq!(
            state.exit_boot_services(key + 1),
            EfiStatus::INVALID_PARAMETER
        );
        assert!(!state.exited());
        assert_eq!(state.exit_boot_services(key), EfiStatus::SUCCESS);
        assert!(state.exited());
        assert_eq!(state.exit_boot_services(key), EfiStatus::UNSUPPORTED);
    }

    #[test]
    fn counts_monotonically() {
        let mut state = state(false);
        assert_eq!(state.next_monotonic_count(), 1);
        assert_eq!(state.next_monotonic_count(), 2);
    }

    #[test]
    fn load_file_sizes_and_copies_the_initrd() {
        assert_eq!(state(false).load_file(Some(0x2000)), LoadFile::NotFound);
        let state = state(true);
        assert!(state.has_initrd());
        assert_eq!(state.load_file(None), LoadFile::TooSmall { size: 0x1234 });
        assert_eq!(
            state.load_file(Some(0x1233)),
            LoadFile::TooSmall { size: 0x1234 }
        );
        assert_eq!(
            state.load_file(Some(0x1234)),
            LoadFile::Copy {
                source: 8 * MB,
                size: 0x1234
            }
        );
    }
}
