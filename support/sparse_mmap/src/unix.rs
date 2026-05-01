// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux implementation for memory mapping abstractions.

#![cfg(unix)]

use pal::unix::SyscallResult;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::io::Error;
use std::os::unix::prelude::*;
use std::ptr::null_mut;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

pub(crate) fn page_size() -> usize {
    static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
    let s = PAGE_SIZE.load(Ordering::Relaxed);
    if s != 0 {
        s
    } else {
        let s = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        PAGE_SIZE.store(s, Ordering::Relaxed);
        s
    }
}

/// A reserved virtual address range that may be partially populated with memory
/// mappings.
#[derive(Debug)]
pub struct SparseMapping {
    address: *mut c_void,
    len: usize,
}

/// An owned handle to an OS object that can be mapped into a [`SparseMapping`].
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub type Mappable = OwnedFd;

/// An object that can be mapped into a `SparseMapping`.
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub use std::os::unix::io::AsFd as AsMappableRef;

/// A reference to an object that can be mapped into a [`SparseMapping`].
///
/// On Windows, this is a section handle. On Linux, it is a file descriptor.
pub type MappableRef<'a> = BorrowedFd<'a>;

/// Creates a new mappable from a file.
///
/// N.B. `writable` and `executable` have no effect on Linux.
pub fn new_mappable_from_file(
    file: &File,
    _writable: bool,
    _executable: bool,
) -> io::Result<Mappable> {
    file.as_fd().try_clone_to_owned()
}

// SAFETY: SparseMapping's internal pointer represents an owned virtual address
// range. There is no safety issue accessing this pointer across threads.
unsafe impl Send for SparseMapping {}
// SAFETY: See above comment
unsafe impl Sync for SparseMapping {}

unsafe fn mmap(
    addr: *mut c_void,
    len: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: i64,
) -> Result<*mut c_void, Error> {
    let address = unsafe { libc::mmap(addr, len, prot, flags, fd, offset) };
    if address == libc::MAP_FAILED {
        return Err(Error::last_os_error());
    }
    Ok(address)
}

unsafe fn munmap(addr: *mut c_void, len: usize) -> Result<(), Error> {
    if unsafe { libc::munmap(addr, len) } < 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
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
    pub fn new_with_minimum_alignment(len: usize, minimum_alignment: usize) -> Result<Self, Error> {
        trycopy::initialize_try_copy();

        // Length of 0 return an OS error, so we need to handle it explicitly.
        if len == 0 {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                "length must be greater than 0",
            ));
        }
        if !minimum_alignment.is_power_of_two() {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                "alignment must be a power of two",
            ));
        }

        let size_4k = 4096;
        let size_2m = 0x200000;
        let size_1g = 0x40000000;
        let default_alignment = if len < size_2m {
            size_4k
        } else if len < size_1g {
            size_2m
        } else {
            size_1g
        };
        let alignment = default_alignment.max(minimum_alignment);

        let len = len
            .checked_add(alignment - 1)
            .map(|temp| temp & !(alignment - 1))
            .ok_or_else(|| {
                Error::new(
                    io::ErrorKind::InvalidInput,
                    "length and alignment combination causes overflow",
                )
            })?;

        let alloc_len = len
            .checked_add(alignment)
            .map(|temp| temp - size_4k)
            .ok_or_else(|| {
                Error::new(
                    io::ErrorKind::InvalidInput,
                    "length and alignment combination causes overflow",
                )
            })?;

        // SAFETY: calling mmap to allocate a new range.
        let address = unsafe {
            mmap(
                null_mut(),
                alloc_len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )? as usize
        };
        let aligned_address = (address + alignment - 1) & !(alignment - 1);
        let end = address + alloc_len;
        let aligned_end = aligned_address + len;
        assert!(aligned_end <= end);

        if address != aligned_address {
            // SAFETY: freeing VA just allocated above.
            unsafe { munmap(address as *mut _, aligned_address - address).unwrap() };
        }
        if aligned_end != end {
            // SAFETY: freeing VA just allocated above.
            unsafe { munmap(aligned_end as *mut _, end - aligned_end).unwrap() };
        }
        Ok(Self {
            address: aligned_address as *mut _,
            len,
        })
    }

    /// Returns true if the mapping is local to the current process.
    pub fn is_local(&self) -> bool {
        true
    }

    /// Returns the pointer to the beginning of the sparse mapping.
    pub fn as_ptr(&self) -> *mut c_void {
        self.address
    }

    /// Returns the length of the mapping, in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    fn validate_offset_len(&self, offset: usize, len: usize) -> io::Result<usize> {
        let end = offset.checked_add(len).ok_or(io::ErrorKind::InvalidInput)?;
        let page_size = page_size();
        if !offset.is_multiple_of(page_size) || !end.is_multiple_of(page_size) || end > self.len {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(end)
    }

    /// Allocates private, writable memory at the given offset within the mapping.
    pub fn alloc(&self, offset: usize, len: usize) -> Result<(), Error> {
        // SAFETY: The flags passed in are guaranteed to be valid
        unsafe {
            self.mmap_anonymous(
                offset,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
            )
        }
    }

    /// Maps read-only zero pages at the given offset within the mapping.
    pub fn map_zero(&self, offset: usize, len: usize) -> Result<(), Error> {
        // SAFETY: The flags passed in are guaranteed to be valid
        unsafe { self.mmap_anonymous(offset, len, libc::PROT_READ, libc::MAP_PRIVATE) }
    }

    /// Updates the protection flags of the mapping at the given offset and length
    /// to allow or disallow writes.
    pub fn set_writable(&self, offset: usize, len: usize, allow_writes: bool) -> Result<(), Error> {
        let prot = if allow_writes {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        self.mprotect(offset, len, prot)
    }

    /// Calls `mprotect` on the mapping at the given offset and length, changing
    /// the protection flags to `prot`.
    fn mprotect(&self, offset: usize, len: usize, prot: i32) -> Result<(), Error> {
        self.validate_offset_len(offset, len)?;
        if prot & !(libc::PROT_READ | libc::PROT_WRITE) != 0 {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported protection flags",
            ));
        }
        // SAFETY: The flags and address passed in are guaranteed to be valid.
        unsafe {
            if libc::mprotect(self.address.add(offset), len, prot) < 0 {
                return Err(Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Maps a portion of a file mapping at `offset`.
    pub fn map_file(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsFd,
        file_offset: u64,
        writable: bool,
    ) -> Result<(), Error> {
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // SAFETY: The flags passed in are guaranteed to be valid. MAP_SHARED is required.
        unsafe {
            self.mmap(
                offset,
                len,
                prot,
                libc::MAP_SHARED,
                file_mapping.as_fd(),
                file_offset as i64,
            )
        }
    }

    /// Maps memory into the mapping, passing parameters through to the mmap
    /// syscall.
    ///
    /// # Safety
    ///
    /// This routine is safe to use as long as the caller ensures `map_flags` excludes
    /// any flags that render the memory region non-unmappable (e.g., `MAP_LOCKED`).
    /// Misuse may lead to system resource issues, such as falsely perceived out-of-memory
    /// conditions.
    pub unsafe fn mmap(
        &self,
        offset: usize,
        len: usize,
        prot: i32,
        map_flags: i32,
        fd: impl AsFd,
        file_offset: i64,
    ) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;

        // SAFETY: guaranteed by caller and offset + len checks above
        unsafe {
            let address = self.address.add(offset);
            let mapped_address = mmap(
                address,
                len,
                prot,
                map_flags | libc::MAP_FIXED,
                fd.as_fd().as_raw_fd(),
                file_offset,
            )?;
            assert_eq!(mapped_address, address);
        }
        Ok(())
    }

    /// Maps anonymous memory into the mapping, with parameters for the mmap syscall.
    ///
    /// # Safety
    ///
    /// This routine is safe to use as long as the caller ensures `map_flags` excludes
    /// any flags that render the memory region non-unmappable (e.g., `MAP_LOCKED`).
    /// Misuse may lead to system resource issues, such as falsely perceived out-of-memory
    /// conditions.
    pub unsafe fn mmap_anonymous(
        &self,
        offset: usize,
        len: usize,
        prot: i32,
        map_flags: i32,
    ) -> io::Result<()> {
        let _ = self.validate_offset_len(offset, len)?;

        // SAFETY: guaranteed by caller and offset + len checks above
        unsafe {
            let address = self.address.add(offset);
            let mapped_address = mmap(
                address,
                len,
                prot,
                map_flags | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )?;
            assert_eq!(mapped_address, address);
        }
        Ok(())
    }

    /// Decommits a range of memory, releasing physical pages back to the host.
    ///
    /// The virtual address range remains accessible; the next access will get
    /// fresh zero pages from the kernel.
    pub fn decommit(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: the address and length have been validated above.
        unsafe {
            let addr = self.address.add(offset);
            if libc::madvise(addr, len, libc::MADV_DONTNEED) < 0 {
                return Err(Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Marks a range as eligible for Transparent Huge Pages.
    ///
    /// This calls `madvise(MADV_HUGEPAGE)` so that khugepaged can collapse
    /// small pages into huge pages. Only effective on anonymous mappings.
    #[cfg(target_os = "linux")]
    pub fn madvise_hugepage(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        // SAFETY: the address and length have been validated above.
        unsafe {
            let addr = self.address.add(offset);
            if libc::madvise(addr, len, libc::MADV_HUGEPAGE) < 0 {
                return Err(Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Names an anonymous mapping range so it appears as `[anon:name]` in
    /// `/proc/{pid}/smaps` and related tools.
    ///
    /// Uses `prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME)`. If the prctl fails
    /// (e.g. on older kernels), the error is silently ignored. No-op on
    /// non-Linux platforms.
    #[cfg(target_os = "linux")]
    pub fn set_name(&self, offset: usize, len: usize, name: &str) {
        if len == 0 {
            return;
        }
        if self.validate_offset_len(offset, len).is_err() {
            return;
        }
        let Ok(name) = std::ffi::CString::new(name) else {
            return;
        };
        // SAFETY: address and length are validated, name is a valid CString.
        unsafe {
            libc::prctl(
                libc::PR_SET_VMA,
                libc::PR_SET_VMA_ANON_NAME,
                self.address.add(offset),
                len,
                name.as_ptr(),
            );
        }
    }

    /// Names a mapping range for debugging. No-op on non-Linux Unix platforms.
    #[cfg(not(target_os = "linux"))]
    pub fn set_name(&self, _offset: usize, _len: usize, _name: &str) {}

    /// Commits a range of memory, making it accessible.
    ///
    /// On Linux, this is a no-op because the kernel handles page faults
    /// transparently for anonymous memory.
    pub fn commit(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        Ok(())
    }

    /// Unmaps memory from the mapping.
    pub fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
        let _ = self.validate_offset_len(offset, len)?;

        // Skipping this check would result in the "expect" below
        if len == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }

        // Remap to PROT_NONE to preserve the reservation.
        // SAFETY: guaranteed by caller and offset + len checks above
        unsafe {
            let address = self.address.add(offset);
            let mapped_address = mmap(
                address,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
            .expect("remap to PROT_NONE should not fail (except for low resources)");
            assert_eq!(mapped_address, address);
        }
        Ok(())
    }
}

impl Drop for SparseMapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.address, self.len)
                .syscall_result()
                .expect("unmap should not fail");
        }
    }
}
#[cfg(target_os = "linux")]
fn new_memfd(name: &str, flags: libc::c_uint) -> io::Result<File> {
    let name =
        std::ffi::CString::new(name).map_err(|e| Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: creating and truncating a new file descriptor according to
    // the documented contract.
    unsafe {
        let fd = libc::memfd_create(name.as_ptr(), flags).syscall_result()?;
        Ok(File::from_raw_fd(fd))
    }
}

#[cfg(not(target_os = "linux"))]
fn new_memfd(_name: &str) -> io::Result<File> {
    // Use a random name because shm_open creates objects in a global namespace.
    // A predictable name would allow other processes to collide with or squat
    // on the name. There is not enough room to include the user-provided name.
    let mut rand = [0; 16];
    getrandom::fill(&mut rand).unwrap();
    let mut name = format!("{:x}", u128::from_ne_bytes(rand));
    // macOS limits the name length to 31 bytes, which is sufficient to ensure uniqueness.
    name.truncate(31);
    let name = std::ffi::CString::new(name).unwrap();
    unsafe {
        // Create a new shared memory object.
        let fd = libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_EXCL | libc::O_CREAT)
            .syscall_result()?;
        // Unlink it to make it anonymous.
        let _ = libc::shm_unlink(name.as_ptr());
        Ok(File::from_raw_fd(fd))
    }
}

/// Allocates a mappable shared memory object of `size` bytes.
///
/// `name` labels the memfd so it appears as `/memfd:<name>` in
/// `/proc/{pid}/smaps` on Linux.
pub fn alloc_shared_memory(size: usize, name: &str) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let fd = new_memfd(name, libc::MFD_CLOEXEC)?;
    #[cfg(not(target_os = "linux"))]
    let fd = new_memfd(name)?;
    fd.set_len(size as u64)?;
    Ok(fd.into())
}

/// Allocates a hugetlb mappable shared memory object of `size` bytes.
///
/// If `hugepage_size` is specified, it is encoded in the memfd flags using
/// the Linux `MFD_HUGE_*` convention.
#[cfg(target_os = "linux")]
pub fn alloc_shared_memory_hugetlb(
    size: usize,
    name: &str,
    hugepage_size: Option<usize>,
) -> io::Result<OwnedFd> {
    const MFD_HUGE_SHIFT: libc::c_uint = 26;

    let mut flags = libc::MFD_CLOEXEC | libc::MFD_HUGETLB;
    if let Some(hugepage_size) = hugepage_size {
        if !hugepage_size.is_power_of_two() {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                "hugepage size must be a power of two",
            ));
        }
        flags |= (hugepage_size.trailing_zeros() as libc::c_uint) << MFD_HUGE_SHIFT;
    }

    let fd = new_memfd(name, flags)?;
    let size = libc::off_t::try_from(size).map_err(|_| {
        Error::new(
            io::ErrorKind::InvalidInput,
            "hugetlb allocation size is too large",
        )
    })?;

    // Unlike ftruncate, fallocate forces hugetlb page reservation now, so
    // insufficient hugepage pools fail during guest RAM allocation instead of
    // later when the lazy VA mapper first mmaps the memfd.
    unsafe { libc::fallocate(fd.as_raw_fd(), 0, 0, size).syscall_result()? };
    Ok(fd.into())
}

/// Allocates a hugetlb mappable shared memory object of `size` bytes.
#[cfg(not(target_os = "linux"))]
pub fn alloc_shared_memory_hugetlb(
    _size: usize,
    _name: &str,
    _hugepage_size: Option<usize>,
) -> io::Result<OwnedFd> {
    Err(Error::new(
        io::ErrorKind::Unsupported,
        "hugetlb shared memory is only supported on Linux",
    ))
}
