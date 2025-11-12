// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows implementation for memory mapping abstractions.

#![cfg(windows)]

use Memory::CreateFileMappingW;
use Memory::MEM_COMMIT;
use Memory::MEM_RELEASE;
use Memory::MEM_RESERVE;
use Memory::MEMORY_MAPPED_VIEW_ADDRESS;
use Memory::MapViewOfFile3;
use Memory::PAGE_EXECUTE;
use Memory::PAGE_EXECUTE_READ;
use Memory::PAGE_EXECUTE_READWRITE;
use Memory::PAGE_EXECUTE_WRITECOPY;
use Memory::PAGE_NOACCESS;
use Memory::PAGE_READONLY;
use Memory::PAGE_READWRITE;
use Memory::PAGE_WRITECOPY;
use Memory::SECTION_MAP_READ;
use Memory::SECTION_MAP_WRITE;
use Memory::UnmapViewOfFile2;
use Memory::VirtualAlloc2;
use Memory::VirtualFreeEx;
use pal::windows::BorrowedHandleExt;
use pal::windows::Process;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::io;
use std::io::Error;
use std::os::windows::prelude::*;
use std::ptr::null;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Memory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const PAGE_SIZE: usize = 4096;

pub(crate) fn page_size() -> usize {
    PAGE_SIZE
}

const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x40000;

const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;

trait ProcessExt {
    fn handle(&self) -> RawHandle;
}

impl ProcessExt for Option<&Process> {
    fn handle(&self) -> RawHandle {
        self.map(|p| p.as_handle().as_raw_handle())
            .unwrap_or_else(|| {
                // SAFETY: just returns a fixed handle.
                unsafe { GetCurrentProcess() as RawHandle }
            })
    }
}

unsafe fn virtual_alloc(
    process: Option<&Process>,
    base_address: *mut c_void,
    size: usize,
    allocation_type: u32,
    page_protection: u32,
    extended_parameters: *mut Memory::MEM_EXTENDED_PARAMETER,
    parameter_count: u32,
) -> Result<*mut c_void, Error> {
    let address = unsafe {
        VirtualAlloc2(
            process.handle(),
            base_address,
            size,
            allocation_type,
            page_protection,
            extended_parameters,
            parameter_count,
        )
    };
    if address.is_null() {
        return Err(Error::last_os_error());
    }
    Ok(address)
}

unsafe fn virtual_free(
    process: Option<&Process>,
    address: *mut c_void,
    size: usize,
    flags: u32,
) -> Result<(), Error> {
    if unsafe { VirtualFreeEx(process.handle(), address, size, flags) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

unsafe fn map_view_of_file(
    process: Option<&Process>,
    file_mapping: RawHandle,
    base_address: *mut c_void,
    offset: u64,
    view_size: usize,
    allocation_type: u32,
    page_protection: u32,
) -> Result<*mut c_void, Error> {
    let address = unsafe {
        MapViewOfFile3(
            file_mapping,
            process.handle(),
            base_address,
            offset,
            view_size,
            allocation_type,
            page_protection,
            null_mut(),
            0,
        )
    }
    .Value;
    if address.is_null() {
        return Err(Error::last_os_error());
    }
    Ok(address)
}

unsafe fn unmap_view_of_file(
    process: Option<&Process>,
    address: *mut c_void,
    flags: u32,
) -> Result<(), Error> {
    if unsafe {
        UnmapViewOfFile2(
            process.handle(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: address },
            flags,
        )
    } == 0
    {
        return Err(Error::last_os_error());
    }
    Ok(())
}

/// A mapping within a sparse mapping.
#[derive(Debug, Clone)]
struct Mapping {
    offset: usize,
    end: usize,
    info: MappingInfo,
}

impl Mapping {
    fn set_offset(&mut self, offset: usize) {
        assert!(self.offset <= offset);
        assert!(offset < self.end);
        let delta = offset - self.offset;
        self.offset = offset;
        match &mut self.info {
            MappingInfo::Anonymous => {}
            MappingInfo::Section { file_offset, .. } => {
                *file_offset += delta as u64;
            }
        }
    }

    fn set_end(&mut self, end: usize) {
        assert!(self.offset < end);
        assert!(end <= self.end);
        self.end = end;
    }
}

#[derive(Debug)]
enum MappingInfo {
    Anonymous,
    Section {
        handle: OwnedHandle,
        file_offset: u64,
        protection: u32,
    },
}

impl Clone for MappingInfo {
    fn clone(&self) -> Self {
        match self {
            Self::Anonymous => Self::Anonymous,
            Self::Section {
                handle,
                file_offset,
                protection,
            } => Self::Section {
                handle: handle.try_clone().unwrap(),
                file_offset: *file_offset,
                protection: *protection,
            },
        }
    }
}

/// A reserved virtual address range that may be partially populated with memory
/// mappings and allocations.
#[derive(Debug)]
pub struct SparseMapping {
    address: *mut c_void,
    len: usize,
    /// The sorted list of mappings. Each unmapped region between mappings and
    /// at the beginning and end of the range must be backed by a single
    /// placeholder reservation.
    mappings: Mutex<MappingList>,

    process: Option<Process>,
}

// SAFETY: SparseMapping's internal pointer represents an owned virtual address
// range. There is no safety issue accessing this pointer across threads.
unsafe impl Send for SparseMapping {}
unsafe impl Sync for SparseMapping {}

/// An owned handle to an OS object that can be mapped into a [`SparseMapping`].
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub type Mappable = OwnedHandle;

/// An object that can be mapped into a `SparseMapping`.
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub use std::os::windows::io::AsHandle as AsMappableRef;

/// A reference to an object that can be mapped into a [`SparseMapping`].
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub type MappableRef<'a> = BorrowedHandle<'a>;

pub fn new_mappable_from_file(
    file: &std::fs::File,
    writable: bool,
    executable: bool,
) -> io::Result<Mappable> {
    let protection = if writable {
        if executable {
            PAGE_EXECUTE_READWRITE
        } else {
            PAGE_READWRITE
        }
    } else {
        if executable {
            PAGE_EXECUTE_READ
        } else {
            PAGE_READONLY
        }
    };

    unsafe {
        let section = CreateFileMappingW(file.as_raw_handle(), null_mut(), protection, 0, 0, null())
            as RawHandle;
        if section.is_null() {
            return Err(Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(section))
    }
}

#[derive(Debug, Default)]
struct MappingList(Vec<Mapping>);

impl MappingList {
    /// Computes the beginning and ending offset of the placeholder that should
    /// exist before the mapping with index `index`, or the end of the sparse
    /// mapping if `index` is out of range.
    fn previous_gap(&self, index: usize, len: usize) -> (usize, usize) {
        let previous_end = if index == 0 { 0 } else { self.0[index - 1].end };
        let next_begin = if index >= self.0.len() {
            len
        } else {
            self.0[index].offset
        };
        (previous_end, next_begin)
    }
}

impl SparseMapping {
    /// Reserves a sparse mapping range with the given size.
    pub fn new(len: usize) -> Result<Self, Error> {
        trycopy::initialize_try_copy();
        Self::new_inner(None, None, len)
    }

    /// Reserves a sparse mapping range with the given address and size in a
    /// remote process.
    pub fn new_remote(
        process: Process,
        address: Option<*mut c_void>,
        len: usize,
    ) -> Result<Self, Error> {
        Self::new_inner(Some(process), address, len)
    }

    fn new_inner(
        process: Option<Process>,
        address: Option<*mut c_void>,
        len: usize,
    ) -> Result<Self, Error> {
        unsafe {
            // Allocate a placeholder reservation to reserve a virtual address
            // range. This will be split up and recombined as mappings come and
            // go.
            let address = virtual_alloc(
                process.as_ref(),
                address.unwrap_or(null_mut()),
                len,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                null_mut(),
                0,
            )?;
            Ok(Self {
                address,
                len,
                mappings: Default::default(),
                process,
            })
        }
    }

    /// Returns true if the mapping is local to the current process.
    pub fn is_local(&self) -> bool {
        self.process.is_none()
    }

    /// Returns the pointer to the beginning of the sparse mapping.
    pub fn as_ptr(&self) -> *mut c_void {
        self.address
    }

    /// Returns the length of the mapping, in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the process associated with the mapping
    pub fn process(&self) -> Option<&Process> {
        self.process.as_ref()
    }

    /// Coalesces placeholder reservations with the given beginning and ending
    /// offset.
    fn coalesce(&self, offset: usize, end: usize) {
        unsafe {
            virtual_free(
                self.process.as_ref(),
                self.address.add(offset),
                end - offset,
                MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS,
            )
            .expect("failed to coalesce placeholders");
        }
    }

    /// Allocates private, writable memory at the given offset within the
    /// mapping.
    pub fn alloc(&self, offset: usize, len: usize) -> Result<(), Error> {
        self.virtual_alloc(offset, len, PAGE_READWRITE)
    }

    /// Maps read-only zero pages at the given offset within the mapping.
    pub fn map_zero(&self, offset: usize, len: usize) -> Result<(), Error> {
        self.virtual_alloc(offset, len, PAGE_READONLY)
    }

    fn validate_offset_len(&self, offset: usize, len: usize) -> io::Result<usize> {
        let end = offset.checked_add(len).ok_or(io::ErrorKind::InvalidInput)?;
        if !offset.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) || end > self.len {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(end)
    }

    /// Creates a new mapping. `f` returns whether the mapping should be freed
    /// with `VirtualFree` (false) or `UnmapViewOfFile` (true).
    fn map<F>(&self, offset: usize, len: usize, f: F) -> Result<(), Error>
    where
        F: FnOnce(*mut c_void) -> Result<MappingInfo, Error>,
    {
        let end = self.validate_offset_len(offset, len)?;
        let mut mappings = self.mappings.lock();

        // Remove the old mappings first. Note that this means the mapping will
        // briefly be missing entirely; accessors need to handle this by
        // retrying.
        let index = self.unmap_internal(&mut mappings, offset, end);

        // Split the placeholder reservation if needed.
        let address = self.address.wrapping_add(offset);
        let (previous_end, next_begin) = mappings.previous_gap(index, self.len);
        if offset > previous_end || end < next_begin {
            unsafe {
                virtual_free(
                    self.process.as_ref(),
                    address,
                    len,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )?;
            }
        }

        match f(address) {
            Ok(info) => {
                mappings.0.insert(index, Mapping { offset, end, info });
                Ok(())
            }
            Err(err) => {
                // TODO: try to restore the old mappings (not really possible in the generic case)

                // Undo the placeholder split.
                if offset > previous_end || end < next_begin {
                    self.coalesce(previous_end, next_begin);
                }
                Err(err)
            }
        }
    }

    /// Allocates private memory at the given offset with memory protection
    /// `protect`.
    pub fn virtual_alloc(&self, offset: usize, len: usize, protect: u32) -> Result<(), Error> {
        self.map(offset, len, |addr| unsafe {
            virtual_alloc(
                self.process.as_ref(),
                addr,
                len,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                protect,
                null_mut(),
                0,
            )?;
            Ok(MappingInfo::Anonymous)
        })
    }

    /// Maps a portion of a file mapping at `offset`.
    pub fn map_file(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        writable: bool,
    ) -> Result<(), Error> {
        let protect = if writable {
            PAGE_READWRITE
        } else {
            PAGE_READONLY
        };
        self.map_view_of_file(offset, len, file_mapping.as_handle(), file_offset, protect)
    }

    /// Maps a portion of a file mapping at `offset` with protection `protect`.
    pub fn map_view_of_file(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        protect: u32,
    ) -> Result<(), Error> {
        assert_ne!(len, 0);
        self.map(offset, len, |addr| unsafe {
            let access = match protect & 0xff {
                PAGE_NOACCESS => 0,
                PAGE_READONLY
                | PAGE_WRITECOPY
                | PAGE_EXECUTE
                | PAGE_EXECUTE_READ
                | PAGE_EXECUTE_WRITECOPY => SECTION_MAP_READ,
                PAGE_READWRITE | PAGE_EXECUTE_READWRITE => SECTION_MAP_READ | SECTION_MAP_WRITE,
                p => panic!("unknown protection {:#x}", p),
            };
            let section = file_mapping.as_handle().duplicate(false, Some(access))?;
            map_view_of_file(
                self.process.as_ref(),
                file_mapping.as_handle().as_raw_handle(),
                addr,
                file_offset,
                len,
                MEM_REPLACE_PLACEHOLDER,
                protect,
            )?;
            Ok(MappingInfo::Section {
                handle: section,
                file_offset,
                protection: protect,
            })
        })
    }

    fn unmap_single(&self, mapping: &Mapping, offset: usize, end: usize) {
        assert!(offset >= mapping.offset);
        assert!(end <= mapping.end);
        unsafe {
            match &mapping.info {
                MappingInfo::Anonymous => {
                    virtual_free(
                        self.process.as_ref(),
                        self.address.wrapping_add(offset),
                        end - offset,
                        MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                    )
                    .expect("failed to free");
                }
                MappingInfo::Section {
                    handle,
                    file_offset,
                    protection,
                } => {
                    // Windows does not support doing partial unmaps. So do our best
                    // to remap, panicking if anything goes wrong.
                    unmap_view_of_file(
                        self.process.as_ref(),
                        self.address.wrapping_add(mapping.offset),
                        MEM_PRESERVE_PLACEHOLDER,
                    )
                    .expect("failed to unmap");

                    if offset > mapping.offset {
                        // Split the placeholder and remap the beginning.
                        let address = self.address.wrapping_add(mapping.offset);
                        let len = offset - mapping.offset;
                        virtual_free(
                            self.process.as_ref(),
                            address,
                            len,
                            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                        )
                        .expect("oom splitting placeholder");

                        map_view_of_file(
                            self.process.as_ref(),
                            handle.as_raw_handle(),
                            address,
                            *file_offset,
                            len,
                            MEM_REPLACE_PLACEHOLDER,
                            *protection,
                        )
                        .expect("remap failed");
                    }

                    if end < mapping.end {
                        // Split the placeholder and remap the end.
                        let address = self.address.wrapping_add(end);
                        let len = mapping.end - end;
                        virtual_free(
                            self.process.as_ref(),
                            address,
                            len,
                            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                        )
                        .expect("oom splitting placeholder");

                        map_view_of_file(
                            self.process.as_ref(),
                            handle.as_raw_handle(),
                            address,
                            *file_offset + (end - mapping.offset) as u64,
                            len,
                            MEM_REPLACE_PLACEHOLDER,
                            *protection,
                        )
                        .expect("remap failed");
                    }
                }
            }
        }
    }

    fn unmap_internal(&self, mappings: &mut MappingList, offset: usize, end: usize) -> usize {
        let index = mappings
            .0
            .binary_search_by_key(&offset, |m| m.end - 1)
            .expect_err("offset is page aligned so cannot equal any end - 1");

        if index == mappings.0.len() {
            return index;
        }

        let mapping = &mut mappings.0[index];
        if offset > mapping.offset && end < mapping.end {
            // Split a single mapping
            self.unmap_single(mapping, offset, end);

            let mut new_mapping = mapping.clone();
            new_mapping.set_offset(end);
            mapping.set_end(offset);
            mappings.0.insert(index + 1, new_mapping);
            return index + 1;
        }

        let mut start_index = index;
        let mut removed = 0;
        let mut unmaps = 0;
        let mut unmapped_len = 0;
        for mapping in &mut mappings.0[index..] {
            assert!(offset < mapping.end);

            if mapping.offset >= end {
                break;
            }

            let (this_offset, this_end) = if offset > mapping.offset {
                start_index += 1;
                (offset, mapping.end)
            } else if end < mapping.end {
                (mapping.offset, end)
            } else {
                removed += 1;
                (mapping.offset, mapping.end)
            };

            self.unmap_single(mapping, this_offset, this_end);
            unmaps += 1;
            unmapped_len += this_end - this_offset;

            if offset > mapping.offset {
                mapping.set_end(offset);
            } else if end < mapping.end {
                mapping.set_offset(end);
            }
        }

        mappings.0.drain(start_index..start_index + removed);

        let (coalesce_offset, coalesce_end) = mappings.previous_gap(start_index, self.len);
        if (unmaps > 0 && coalesce_end - coalesce_offset > unmapped_len) || unmaps > 1 {
            self.coalesce(coalesce_offset, coalesce_end);
        }

        start_index
    }

    /// Unmaps a range of mappings.
    pub fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
        let end = self.validate_offset_len(offset, len)?;
        let mut mappings = self.mappings.lock();
        self.unmap_internal(&mut mappings, offset, end);
        Ok(())
    }
}

impl Drop for SparseMapping {
    fn drop(&mut self) {
        self.unmap_internal(&mut self.mappings.lock(), 0, self.len);
        unsafe {
            virtual_free(self.process.as_ref(), self.address, 0, MEM_RELEASE)
                .expect("placeholder free failed");
        }
    }
}

/// Allocates a mappable shared memory object of `size` bytes.
pub fn alloc_shared_memory(size: usize) -> io::Result<OwnedHandle> {
    // SAFETY: calling according to API
    unsafe {
        let h = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            null_mut(),
            PAGE_READWRITE,
            (size >> 32) as u32,
            size as u32,
            null(),
        ) as RawHandle;
        if h.is_null() {
            return Err(Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(h))
    }
}

#[cfg(test)]
mod tests {
    use super::SparseMapping;
    use super::alloc_shared_memory;
    use trycopy::try_copy;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    #[test]
    fn test_shared_mem_split() {
        trycopy::initialize_try_copy();

        let shmem = alloc_shared_memory(0x100000).unwrap();
        let sparse = SparseMapping::new(0x100000).unwrap();
        sparse
            .map_view_of_file(0, 0x100000, &shmem, 0, PAGE_READWRITE)
            .unwrap();
        let data: &mut [u32] =
            unsafe { std::slice::from_raw_parts_mut(sparse.as_ptr().cast(), sparse.len() / 4) };
        for (i, d) in data.iter_mut().enumerate() {
            *d = i as u32 * 4;
        }
        let check = |offset: usize| {
            let mut d: u32 = 0;
            unsafe {
                try_copy(
                    sparse.as_ptr().wrapping_add(offset),
                    std::ptr::from_mut(&mut d).cast(),
                    4,
                )
                .unwrap();
            }
            assert_eq!(d, offset as u32);
        };
        check(0x5000);
        sparse.unmap(0x40000, 0x2000).unwrap();
        check(0x30000);
        check(0x50000);
        sparse.unmap(0, 0x1000).unwrap();
        check(0x1000);
        sparse.unmap(0xf0000, 0x10000).unwrap();
        check(0xef000);
    }

    #[test]
    fn test_remote() {
        let process = pal::windows::process::empty_process().unwrap();
        let shmem = alloc_shared_memory(0x100000).unwrap();
        let sparse = SparseMapping::new_remote(process.process, None, 0x100000).unwrap();
        sparse.map_file(0, 0x10000, &shmem, 0, true).unwrap();

        let process_addr = pal::windows::process::empty_process().unwrap();
        let sparse_addr = SparseMapping::new_remote(
            process_addr.process,
            Some(0x100000 as *mut std::ffi::c_void),
            0x100000,
        )
        .unwrap();
        sparse_addr.map_file(0, 0x10000, &shmem, 0, true).unwrap();
    }
}
