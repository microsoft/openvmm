// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows implementation for memory mapping abstractions.

#![cfg(windows)]

use Memory::CreateFileMappingNumaW;
use Memory::CreateFileMappingW;
use Memory::GetLargePageMinimum;
use Memory::MEM_COMMIT;
use Memory::MEM_DECOMMIT;
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
use Memory::PrefetchVirtualMemory;
use Memory::SEC_COMMIT;
use Memory::SEC_LARGE_PAGES;
use Memory::SECTION_MAP_READ;
use Memory::SECTION_MAP_WRITE;
use Memory::UnmapViewOfFile2;
use Memory::VirtualAlloc2;
use Memory::VirtualFreeEx;
use Memory::VirtualProtectEx;
use Memory::WIN32_MEMORY_RANGE_ENTRY;
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
use windows_sys::Win32::System::SystemServices::NUMA_NO_PREFERRED_NODE;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The system page size: the unit at which memory is committed and protection
/// is applied.
///
/// `GetSystemInfo` reports the actual value, but every architecture Windows
/// runs on (x86, x64, and ARM64) uses 4 KB pages, so we hardcode it rather than
/// querying it at runtime.
const PAGE_SIZE: usize = 4096;

/// The Windows *allocation granularity*.
///
/// Windows has two distinct memory sizes. The page size (4 KB) is the unit at
/// which memory is committed and protection is applied. The *allocation
/// granularity* is the coarser unit — 64 KB — that the base address of every
/// virtual-address *reservation* must be aligned to: `VirtualAlloc`,
/// `VirtualAlloc2`, `MapViewOfFile3`, etc. round the base of a new reservation
/// down to a multiple of this value. (This is separate from, and larger than,
/// the page size; it exists mainly for historical Alpha/portability reasons.)
///
/// `GetSystemInfo` reports the actual value, but it is never larger than 64 KB,
/// so we hardcode that rather than querying it at runtime.
const ALLOCATION_GRANULARITY: usize = 0x10000;

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
    extended_parameters: &mut [Memory::MEM_EXTENDED_PARAMETER],
) -> Result<*mut c_void, Error> {
    let (params_ptr, params_count) = if extended_parameters.is_empty() {
        (null_mut(), 0)
    } else {
        (
            extended_parameters.as_mut_ptr(),
            extended_parameters.len() as u32,
        )
    };
    let address = unsafe {
        VirtualAlloc2(
            process.handle(),
            base_address,
            size,
            allocation_type,
            page_protection,
            params_ptr,
            params_count,
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

unsafe fn virtual_protect(
    process: Option<&Process>,
    address: *mut c_void,
    size: usize,
    page_protection: u32,
) -> Result<(), Error> {
    let mut old_protection = 0;
    // SAFETY: caller guarantees `address..address + size` is a committed range
    // within a mapping owned by this `SparseMapping`.
    if unsafe {
        VirtualProtectEx(
            process.handle(),
            address,
            size,
            page_protection,
            &mut old_protection,
        )
    } == 0
    {
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
    extended_parameters: &mut [Memory::MEM_EXTENDED_PARAMETER],
) -> Result<*mut c_void, Error> {
    let (params_ptr, params_count) = if extended_parameters.is_empty() {
        (null_mut(), 0)
    } else {
        (
            extended_parameters.as_mut_ptr(),
            extended_parameters.len() as u32,
        )
    };
    let address = unsafe {
        MapViewOfFile3(
            file_mapping,
            process.handle(),
            base_address,
            offset,
            view_size,
            allocation_type,
            page_protection,
            params_ptr,
            params_count,
        )
    }
    .Value;
    if address.is_null() {
        return Err(Error::last_os_error());
    }
    Ok(address)
}

/// Returns a NUMA node `MEM_EXTENDED_PARAMETER`, if a node is specified.
///
/// See <https://learn.microsoft.com/windows/win32/api/winnt/ns-winnt-mem_extended_parameter>
fn numa_extended_param(numa_node: Option<u32>) -> Option<Memory::MEM_EXTENDED_PARAMETER> {
    let node = numa_node?;
    let mut param = Memory::MEM_EXTENDED_PARAMETER::default();
    // The `_bitfield` layout is: Type in the low 8 bits, Reserved in the
    // upper 56 bits. windows-rs 0.62 does not expose typed accessors for
    // this bitfield struct, so we set it directly.
    param.Anonymous1._bitfield = Memory::MemExtendedParameterNumaNode as u64 & 0xff;
    param.Anonymous2.ULong = node;
    Some(param)
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
    ///
    /// The range will be aligned to the largest system page size that's smaller
    /// or equal to `len`.
    pub fn new(len: usize) -> Result<Self, Error> {
        Self::new_with_minimum_alignment(len, 1)
    }

    /// Reserves a sparse mapping range with at least the requested alignment.
    ///
    /// The range will be aligned to the larger of `minimum_alignment` and the
    /// largest system page size that's smaller or equal to `len`.
    ///
    /// Alignments up to the Windows allocation granularity (64 KB) are
    /// satisfied implicitly by the reservation. Larger alignments (e.g. 2 MB
    /// for large-page backing) are requested explicitly via a
    /// `MEM_ADDRESS_REQUIREMENTS` extended parameter.
    pub fn new_with_minimum_alignment(len: usize, minimum_alignment: usize) -> Result<Self, Error> {
        trycopy::initialize_try_copy();

        // Pick a default alignment based on the mapping size so that larger
        // mappings land on large-page boundaries, matching the Linux backend.
        let alignment = crate::reservation_alignment(len, minimum_alignment)?;
        Self::new_inner(None, None, len, alignment)
    }

    /// Reserves a sparse mapping range with the given address and size in a
    /// remote process.
    ///
    /// As with [`Self::new_with_minimum_alignment`], the range is aligned to
    /// the larger of `minimum_alignment` and the largest system page size
    /// that's smaller or equal to `len`, so that large mappings can back large
    /// pages. When an explicit `address` is provided the caller controls
    /// placement, so no additional alignment requirement is imposed.
    pub fn new_remote(
        process: Process,
        address: Option<*mut c_void>,
        len: usize,
        minimum_alignment: usize,
    ) -> Result<Self, Error> {
        let alignment = crate::reservation_alignment(len, minimum_alignment)?;
        Self::new_inner(Some(process), address, len, alignment)
    }

    fn new_inner(
        process: Option<Process>,
        address: Option<*mut c_void>,
        len: usize,
        alignment: usize,
    ) -> Result<Self, Error> {
        // Only alignments larger than the allocation granularity need an
        // explicit address requirement; smaller alignments are satisfied
        // implicitly by the reservation base. This also keeps
        // MEM_ADDRESS_REQUIREMENTS.Alignment valid, since it must be zero or a
        // power of two that is at least the allocation granularity. An address
        // requirement is mutually exclusive with an explicit base address, so
        // skip it when the caller chose the address.
        let requirement =
            (address.is_none() && alignment > ALLOCATION_GRANULARITY).then_some(alignment);
        unsafe {
            let mut requirements = Memory::MEM_ADDRESS_REQUIREMENTS {
                LowestStartingAddress: null_mut(),
                HighestEndingAddress: null_mut(),
                Alignment: requirement.unwrap_or(0),
            };
            let mut param = Memory::MEM_EXTENDED_PARAMETER::default();
            param.Anonymous1._bitfield =
                Memory::MemExtendedParameterAddressRequirements as u64 & 0xff;
            param.Anonymous2.Pointer = std::ptr::from_mut(&mut requirements).cast();
            let mut params = [param];
            let extended_parameters: &mut [Memory::MEM_EXTENDED_PARAMETER] =
                if requirement.is_some() {
                    params.as_mut_slice()
                } else {
                    &mut []
                };

            // Allocate a placeholder reservation to reserve a virtual address
            // range. This will be split up and recombined as mappings come and
            // go.
            let address = virtual_alloc(
                process.as_ref(),
                address.unwrap_or(null_mut()),
                len,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                extended_parameters,
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
        self.virtual_alloc(offset, len, PAGE_READWRITE, None)
    }

    /// Allocates private, writable memory at the given offset, optionally
    /// bound to a specific host NUMA node.
    pub fn alloc_numa(
        &self,
        offset: usize,
        len: usize,
        numa_node: Option<u32>,
    ) -> Result<(), Error> {
        self.virtual_alloc(offset, len, PAGE_READWRITE, numa_node)
    }

    /// Changes the page protection of a committed range to `protection` (a
    /// `PAGE_*` flag).
    ///
    /// The range must be committed. This does not allocate physical pages; on
    /// Windows those are still materialized on first write.
    pub fn protect(&self, offset: usize, len: usize, protection: u32) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `validate_offset_len` confirmed the range lies within the
        // reservation.
        unsafe {
            virtual_protect(
                self.process.as_ref(),
                self.address.wrapping_add(offset),
                len,
                protection,
            )
        }
    }

    /// Prefetches a committed range into the working set, faulting the whole
    /// range in with a single call.
    ///
    /// This is the soft-large-page primitive: faulting an entire 2 MB-aligned,
    /// read-write window in one operation gives the OS the opportunity to back
    /// it with a large page, which in turn allows a large second-level address
    /// translation (SLAT) entry for a guest. Faulting the pages individually
    /// instead tends to leave the window backed by scattered small pages. This
    /// is best-effort: if a large page is unavailable, the range is simply
    /// backed by small pages. The range must already be **read-write** —
    /// prefetching a read-only range only maps the shared zero page.
    ///
    /// Uses the documented `PrefetchVirtualMemory` API with a flag that brings
    /// the pages fully into the working set. That flag requires Windows 11;
    /// older releases fail the call, so callers must treat an error as a
    /// best-effort miss (the range stays valid, just not necessarily large-page
    /// backed).
    pub fn prefetch(&self, offset: usize, len: usize) -> Result<(), Error> {
        // `PrefetchVirtualMemory` flag (Windows 11+) that brings the pages fully
        // into the working set rather than only paging in file backing. Not yet
        // exposed as a named constant by the `windows-sys` bindings.
        const VM_PREFETCH_TO_WORKING_SET: u32 = 0x1;

        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        let entry = WIN32_MEMORY_RANGE_ENTRY {
            VirtualAddress: self.address.wrapping_add(offset),
            NumberOfBytes: len,
        };
        // SAFETY: calling per the API contract; `entry` describes a range that
        // `validate_offset_len` confirmed lies within this reservation.
        let ret = unsafe {
            PrefetchVirtualMemory(
                self.process.as_ref().handle(),
                1,
                &entry,
                VM_PREFETCH_TO_WORKING_SET,
            )
        };
        if ret == 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    /// Maps read-only zero pages at the given offset within the mapping.
    pub fn map_zero(&self, offset: usize, len: usize) -> Result<(), Error> {
        self.virtual_alloc(offset, len, PAGE_READONLY, None)
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
    /// `protect`, optionally bound to a specific host NUMA node.
    pub fn virtual_alloc(
        &self,
        offset: usize,
        len: usize,
        protect: u32,
        numa_node: Option<u32>,
    ) -> Result<(), Error> {
        self.map(offset, len, |addr| unsafe {
            let mut param = numa_extended_param(numa_node);
            virtual_alloc(
                self.process.as_ref(),
                addr,
                len,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                protect,
                param.as_mut_slice(),
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
        self.map_view_of_file(
            offset,
            len,
            file_mapping.as_handle(),
            file_offset,
            protect,
            None,
        )
    }

    /// Maps a portion of a file mapping at `offset`, optionally bound to a
    /// specific host NUMA node.
    pub fn map_file_numa(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        writable: bool,
        numa_node: Option<u32>,
    ) -> Result<(), Error> {
        let protect = if writable {
            PAGE_READWRITE
        } else {
            PAGE_READONLY
        };
        self.map_view_of_file(
            offset,
            len,
            file_mapping.as_handle(),
            file_offset,
            protect,
            numa_node,
        )
    }

    /// Maps a portion of a file mapping at `offset` with protection `protect`,
    /// optionally bound to a specific host NUMA node.
    pub fn map_view_of_file(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        protect: u32,
        numa_node: Option<u32>,
    ) -> Result<(), Error> {
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
        self.map_view_of_file_access(
            offset,
            len,
            file_mapping,
            file_offset,
            protect,
            access,
            numa_node,
        )
    }

    /// Maps a portion of a file mapping with an explicit section `access`
    /// (`SECTION_MAP_*`) and page `protect` (`PAGE_*`), optionally bound to a
    /// specific host NUMA node.
    ///
    /// Unlike [`map_view_of_file`](Self::map_view_of_file), which derives the
    /// section access from the page protection, this lets the caller request
    /// broader section access than the initial protection — e.g. a read-only
    /// view backed by a write-capable section, so the view can later be raised
    /// to read-write via [`protect`](Self::protect).
    pub fn map_view_of_file_access(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        protect: u32,
        access: u32,
        numa_node: Option<u32>,
    ) -> Result<(), Error> {
        assert_ne!(len, 0);
        self.map(offset, len, |addr| unsafe {
            // Keep a handle for the mapping's lifetime (unmap/split re-map
            // through it), restricted to just `access` as mild defense in depth.
            // Either handle works for the initial map below (same section), so
            // use the caller's original.
            let section = file_mapping.as_handle().duplicate(false, Some(access))?;
            let mut param = numa_extended_param(numa_node);
            map_view_of_file(
                self.process.as_ref(),
                file_mapping.as_handle().as_raw_handle(),
                addr,
                file_offset,
                len,
                MEM_REPLACE_PLACEHOLDER,
                protect,
                param.as_mut_slice(),
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
                            &mut [],
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
                            &mut [],
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

    /// Decommits a range of memory, releasing physical pages back to the host.
    ///
    /// The virtual address range remains reserved; accessing decommitted
    /// pages will cause an access violation until they are recommitted
    /// with [`commit()`](Self::commit).
    ///
    /// This is only valid for ranges that were previously committed with
    /// [`alloc()`](Self::alloc) or [`commit()`](Self::commit).
    pub fn decommit(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        unsafe {
            virtual_free(
                self.process.as_ref(),
                self.address.wrapping_add(offset),
                len,
                MEM_DECOMMIT,
            )
        }
    }

    /// Commits a range of previously reserved or decommitted memory.
    ///
    /// This is used to recommit pages after [`decommit()`](Self::decommit).
    /// For the initial commit of anonymous pages (replacing placeholders),
    /// use [`alloc()`](Self::alloc) instead.
    ///
    /// Committing already-committed pages is a no-op.
    pub fn commit(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        unsafe {
            virtual_alloc(
                self.process.as_ref(),
                self.address.wrapping_add(offset),
                len,
                MEM_COMMIT,
                PAGE_READWRITE,
                &mut [],
            )?;
        }
        Ok(())
    }

    /// Names a mapping range for debugging. No-op on Windows.
    pub fn set_name(&self, _offset: usize, _len: usize, _name: &str) {}

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
///
/// `name` is used for debugging on Linux; ignored on Windows.
pub fn alloc_shared_memory(size: usize, _name: &str) -> io::Result<OwnedHandle> {
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

/// Allocates a hugetlb mappable shared memory object of `size` bytes.
///
/// On Windows this creates a large-page section (`SEC_LARGE_PAGES`). Only the
/// large-page minimum size reported by `GetLargePageMinimum` (2 MB on x64) is
/// supported; any other `hugepage_size` is rejected. The physical memory is
/// allocated and pinned immediately, so allocation fails (rather than falling
/// back to small pages) if sufficient contiguous physical memory is
/// unavailable. Requires `SeLockMemoryPrivilege` (the "Lock pages in memory"
/// user right), which is enabled on a thread-scoped impersonation token only
/// for the duration of the section-creation call, so the process token is left
/// unchanged.
pub fn alloc_shared_memory_hugetlb(
    size: usize,
    _name: &str,
    hugepage_size: Option<usize>,
    numa_node: Option<u32>,
) -> io::Result<OwnedHandle> {
    // SAFETY: no preconditions.
    let large_page_minimum = unsafe { GetLargePageMinimum() };
    if large_page_minimum == 0 {
        return Err(Error::new(
            io::ErrorKind::Unsupported,
            "large pages are not supported by this system",
        ));
    }

    if let Some(hugepage_size) = hugepage_size {
        if hugepage_size != large_page_minimum {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unsupported hugepage size {hugepage_size:#x}; Windows large-page sections only support the large-page minimum ({large_page_minimum:#x})"
                ),
            ));
        }
    }

    if !size.is_multiple_of(large_page_minimum) {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "large-page allocation size {size:#x} is not a multiple of the large-page minimum ({large_page_minimum:#x})"
            ),
        ));
    }

    // The physical pages backing a SEC_LARGE_PAGES section are allocated and
    // pinned at creation time, so SeLockMemoryPrivilege must be enabled for the
    // CreateFileMapping call. Scope it to this thread (via an impersonation
    // token) so the privilege is not left enabled process-wide, which would
    // race with concurrent large-page allocations on other threads.
    with_lock_memory_privilege(|| {
        // SAFETY: calling according to API
        let h = unsafe {
            CreateFileMappingNumaW(
                INVALID_HANDLE_VALUE,
                null_mut(),
                PAGE_READWRITE | SEC_COMMIT | SEC_LARGE_PAGES,
                (size >> 32) as u32,
                size as u32,
                null(),
                numa_node.unwrap_or(NUMA_NO_PREFERRED_NODE),
            )
        } as RawHandle;
        if h.is_null() {
            return Err(Error::last_os_error());
        }
        // SAFETY: `h` is a freshly created, owned section handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    })
}

/// Runs `f` with `SeLockMemoryPrivilege` ("Lock pages in memory") enabled on a
/// thread-scoped impersonation token, then reverts the thread to its normal
/// security context.
///
/// The privilege is enabled on a private per-thread copy of the process token
/// rather than on the process token itself, so it is never left enabled
/// process-wide -- which would broaden the enabled window unnecessarily and
/// race with concurrent large-page allocations on other threads.
///
/// Fails loudly if the privilege is not held, rather than silently allowing the
/// large-page allocation in `f` to fail with a less actionable error.
fn with_lock_memory_privilege<R>(f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
    use windows_sys::Wdk::System::SystemServices::SE_LOCK_MEMORY_PRIVILEGE;
    use windows_sys::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::AdjustTokenPrivileges;
    use windows_sys::Win32::Security::ImpersonateSelf;
    use windows_sys::Win32::Security::LUID_AND_ATTRIBUTES;
    use windows_sys::Win32::Security::RevertToSelf;
    use windows_sys::Win32::Security::SE_PRIVILEGE_ENABLED;
    use windows_sys::Win32::Security::SecurityImpersonation;
    use windows_sys::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
    use windows_sys::Win32::Security::TOKEN_PRIVILEGES;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::OpenThreadToken;

    /// Reverts the calling thread to its process security context on drop, so
    /// the impersonation token is removed even if `f` panics or an early return
    /// occurs after impersonation is established.
    struct RevertToSelfGuard;
    impl Drop for RevertToSelfGuard {
        fn drop(&mut self) {
            // SAFETY: no preconditions.
            if unsafe { RevertToSelf() } == 0 {
                panic!(
                    "failed to revert thread impersonation: {}",
                    Error::last_os_error()
                );
            }
        }
    }

    // Give the current thread an impersonation token copied from the process
    // token, so that the privilege change below is scoped to this thread.
    // SAFETY: calling per API contract.
    if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
        return Err(Error::last_os_error());
    }
    let _revert = RevertToSelfGuard;

    // Open this thread's impersonation token for privilege adjustment.
    // SAFETY: calling per API contract; the returned token handle is owned and
    // closed on drop.
    let token = unsafe {
        let mut token = null_mut();
        if OpenThreadToken(
            GetCurrentThread(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            // Open using this thread's (impersonation) context.
            0,
            &mut token,
        ) == 0
        {
            return Err(Error::last_os_error());
        }
        OwnedHandle::from_raw_handle(token as RawHandle)
    };

    let tkp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: LUID {
                LowPart: SE_LOCK_MEMORY_PRIVILEGE as u32,
                HighPart: 0,
            },
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };

    // SAFETY: calling per API contract with an initialized privileges struct.
    let r =
        unsafe { AdjustTokenPrivileges(token.as_raw_handle(), 0, &tkp, 0, null_mut(), null_mut()) };
    if r == 0 {
        return Err(Error::last_os_error());
    }

    // AdjustTokenPrivileges returns success even when the token does not hold
    // the requested privilege; the failure is reported via GetLastError.
    // SAFETY: no preconditions; no intervening calls clobber the last error.
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(Error::new(
            io::ErrorKind::PermissionDenied,
            "SeLockMemoryPrivilege is not held",
        ));
    }

    f()
}

#[cfg(test)]
mod tests {
    use super::GetLargePageMinimum;
    use super::PAGE_SIZE;
    use super::SparseMapping;
    use super::alloc_shared_memory;
    use super::alloc_shared_memory_hugetlb;
    use std::io;
    use trycopy::try_copy;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    #[test]
    fn test_shared_mem_split() {
        trycopy::initialize_try_copy();

        let shmem = alloc_shared_memory(0x100000, "test").unwrap();
        let sparse = SparseMapping::new(0x100000).unwrap();
        sparse
            .map_view_of_file(0, 0x100000, &shmem, 0, PAGE_READWRITE, None)
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
        let shmem = alloc_shared_memory(0x100000, "test").unwrap();
        let sparse = SparseMapping::new_remote(process.process, None, 0x100000, 1).unwrap();
        sparse.map_file(0, 0x10000, &shmem, 0, true).unwrap();

        let process_addr = pal::windows::process::empty_process().unwrap();
        let sparse_addr = SparseMapping::new_remote(
            process_addr.process,
            Some(0x100000 as *mut std::ffi::c_void),
            0x100000,
            1,
        )
        .unwrap();
        sparse_addr.map_file(0, 0x10000, &shmem, 0, true).unwrap();
    }

    /// Rejects hugepage sizes and allocation sizes that are not the large-page
    /// minimum before any privileged allocation is attempted, so this needs no
    /// special privilege and runs in CI.
    #[test]
    fn test_large_page_rejects_bad_size() {
        // SAFETY: no preconditions.
        let large_page_minimum = unsafe { GetLargePageMinimum() };
        if large_page_minimum == 0 {
            // Large pages are unsupported on this system; nothing to validate.
            return;
        }

        // A hugepage size other than the large-page minimum is rejected up
        // front (before SeLockMemoryPrivilege is ever needed).
        let err = alloc_shared_memory_hugetlb(
            large_page_minimum,
            "test",
            Some(large_page_minimum * 2),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // A size that is not a multiple of the large-page minimum is likewise
        // rejected up front.
        let err = alloc_shared_memory_hugetlb(large_page_minimum + PAGE_SIZE, "test", None, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Exercises the actual large-page (`SEC_LARGE_PAGES`) allocation and
    /// mapping path. This requires the "Lock pages in memory" user right
    /// (`SeLockMemoryPrivilege`), which is not held by default, so it is
    /// ignored. To run it manually, grant the privilege (see
    /// `scripts/grant-privilege.ps1`), sign out and back in, then run:
    ///
    /// ```text
    /// cargo test -p sparse_mmap -- --ignored large_page_alloc
    /// ```
    #[test]
    #[ignore = "requires SeLockMemoryPrivilege; run manually"]
    fn test_large_page_alloc_and_map() {
        trycopy::initialize_try_copy();

        // SAFETY: no preconditions.
        let large_page_minimum = unsafe { GetLargePageMinimum() };
        assert_ne!(large_page_minimum, 0, "large pages not supported");

        let size = large_page_minimum;
        let shmem = alloc_shared_memory_hugetlb(size, "test", Some(large_page_minimum), None)
            .expect("large-page allocation failed (is SeLockMemoryPrivilege held?)");

        let sparse = SparseMapping::new(size).unwrap();
        sparse
            .map_view_of_file(0, size, &shmem, 0, PAGE_READWRITE, None)
            .unwrap();

        // Round-trip values through the large-page-backed mapping.
        let data: &mut [u32] =
            unsafe { std::slice::from_raw_parts_mut(sparse.as_ptr().cast(), sparse.len() / 4) };
        for (i, d) in data.iter_mut().enumerate() {
            *d = i as u32;
        }
        assert_eq!(data[0], 0);
        assert_eq!(data[size / 4 - 1], (size / 4 - 1) as u32);
    }
}
