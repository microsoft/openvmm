// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::UnsafeCell;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

pub(crate) struct MappedPage<T>(NonNull<UnsafeCell<T>>);

impl<T> MappedPage<T> {
    pub fn new(fd: &File, pg_off: i64) -> io::Result<Self> {
        // Make sure any T we're using can fit in any size page.
        const _: () = assert!(size_of::<T>() <= 4096);

        // SAFETY: calling mmap as documented to create a new mapping.
        let ptr = unsafe {
            let page_size = libc::sysconf(libc::_SC_PAGESIZE);
            libc::mmap(
                std::ptr::null_mut(),
                page_size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                pg_off * page_size,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Self(NonNull::new(ptr).unwrap().cast()))
    }

    pub fn as_ptr(&self) -> *mut T {
        UnsafeCell::raw_get(self.0.as_ptr())
    }

    pub fn as_ref(&self) -> &UnsafeCell<T> {
        // SAFETY: The pointer is valid and mapped for the lifetime of the struct,
        // it will only every point to a T, and UnsafeCell allows interior mutability.
        unsafe { self.0.as_ref() }
    }
}

impl<T> std::fmt::Debug for MappedPage<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MappedPage").field(&self.0).finish()
    }
}

impl<T> Drop for MappedPage<T> {
    fn drop(&mut self) {
        // SAFETY: unmapping memory mapped at construction.
        unsafe {
            libc::munmap(
                self.0.as_ptr().cast(),
                libc::sysconf(libc::_SC_PAGESIZE) as usize,
            );
        }
    }
}

// SAFETY: this is just a pointer value.
unsafe impl<T> Send for MappedPage<T> {}
// SAFETY: see above comment
unsafe impl<T> Sync for MappedPage<T> {}
