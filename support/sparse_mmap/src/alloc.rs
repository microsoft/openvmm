// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory functionality that needs a refactor.

use std::ops::Deref;
use std::ops::DerefMut;
use std::slice;
use std::sync::atomic::AtomicU8;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

#[derive(Debug)]
/// A `Box`-like smart pointer for memory mappings.
/// Relies on the kernel interfaces rather than the
/// standard library API. That allows for the low-level
/// features this crate deals with.
pub struct Allocation {
    ptr: *mut u8,
    size: usize,
    _dummy: std::marker::PhantomData<[u8]>,
}

// SAFETY: memory mappings are for created managing the guest memory
// and expected to live longer than the guest and any threads running
// the guest VPs. Essentially, these are constant when the guest runs.
// Thus the `ptr` field can be sent to other threads.
unsafe impl Send for Allocation {}
// SAFETY: Same as above for the sharing considerations.
unsafe impl Sync for Allocation {}

impl Allocation {
    pub fn new(size: usize) -> Result<Self, std::io::Error> {
        let ptr = sys::alloc(size)?;
        Ok(Allocation {
            ptr,
            size,
            _dummy: std::marker::PhantomData,
        })
    }
}

impl DerefMut for Allocation {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: the pointer is valid.
        unsafe { slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Deref for Allocation {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: the pointer is valid.
        unsafe { slice::from_raw_parts(self.ptr, self.size) }
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: the pointer is valid.
        unsafe {
            sys::free(self.ptr, self.size);
        }
    }
}

#[derive(Debug)]
pub struct SharedMem {
    alloc: Allocation,
}

impl SharedMem {
    pub fn new(alloc: Allocation) -> Self {
        SharedMem { alloc }
    }
}

impl Deref for SharedMem {
    type Target = [AtomicU8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the pointer is valid.
        unsafe { slice::from_raw_parts(self.alloc.ptr as *const AtomicU8, self.alloc.size) }
    }
}

#[cfg(windows)]
mod windows {
    use std::ptr;
    use windows_sys::Win32::System::Memory::VirtualAlloc;
    use windows_sys::Win32::System::Memory::VirtualFree;
    use windows_sys::Win32::System::Memory::MEM_COMMIT;
    use windows_sys::Win32::System::Memory::MEM_RELEASE;
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    pub fn alloc(size: usize) -> std::io::Result<*mut u8> {
        // SAFETY: Calling the system API as required. The parameters have
        // valid values.
        let ptr = unsafe {
            VirtualAlloc(
                ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if ptr.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ptr.cast::<u8>())
    }

    pub unsafe fn free(ptr: *mut u8, _size: usize) {
        // SAFETY: Calling the system API as required. The parameters have
        // valid values.
        let ret = unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
        assert!(ret != 0);
    }
}

#[cfg(unix)]
mod unix {
    use std::ptr;

    pub fn alloc(size: usize) -> std::io::Result<*mut u8> {
        // SAFETY: using the system API as required by the documentation.
        // The values for the parameters are valid.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ptr.cast::<u8>())
    }

    pub unsafe fn free(ptr: *mut u8, size: usize) {
        // SAFETY: using the system API as required by the documentation.
        // The values for the parameters are valid.
        let ret = unsafe { libc::munmap(ptr.cast::<libc::c_void>(), size) };
        assert!(ret == 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_free() -> Result<(), Box<dyn std::error::Error>> {
        // SAFETY: memory is not shared between threads.
        unsafe {
            let x = sys::alloc(4096)?;
            sys::free(x, 4096);
            Ok(())
        }
    }
}
