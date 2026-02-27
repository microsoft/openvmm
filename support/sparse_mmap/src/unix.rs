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
        trycopy::initialize_try_copy();

        // Length of 0 return an OS error, so we need to handle it explicitly.
        if len == 0 {
            return Err(Error::new(
                io::ErrorKind::InvalidInput,
                "length must be greater than 0",
            ));
        }

        let size_4k = 4096;
        let size_2m = 0x200000;
        let size_1g = 0x40000000;
        let alignment = if len < size_2m {
            size_4k
        } else if len < size_1g {
            size_2m
        } else {
            size_1g
        };

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
fn new_memfd() -> io::Result<File> {
    // SAFETY: creating and truncating a new file descriptor according to
    // the documented contract.
    unsafe {
        let fd = libc::memfd_create(c"mem".as_ptr(), libc::MFD_CLOEXEC).syscall_result()?;
        Ok(File::from_raw_fd(fd))
    }
}

#[cfg(not(target_os = "linux"))]
fn new_memfd() -> io::Result<File> {
    let mut name = [0; 16];
    getrandom::fill(&mut name).unwrap();
    let mut name = format!("{:x}", u128::from_ne_bytes(name));
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
pub fn alloc_shared_memory(size: usize) -> io::Result<OwnedFd> {
    let fd = new_memfd()?;
    fd.set_len(size as u64)?;
    Ok(fd.into())
}
