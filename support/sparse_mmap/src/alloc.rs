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
pub struct Allocation {
    ptr: *mut u8,
    size: usize,
    _dummy: std::marker::PhantomData<[u8]>,
}

unsafe impl Send for Allocation {}
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
        unsafe { slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Deref for Allocation {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.size) }
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
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
        unsafe { slice::from_raw_parts(self.alloc.ptr as *const AtomicU8, self.alloc.size) }
    }
}

#[cfg(windows)]
mod windows {
    use std::ptr;
    use windows_sys::Win32::System::Memory::MEM_COMMIT;
    use windows_sys::Win32::System::Memory::MEM_RELEASE;
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    use windows_sys::Win32::System::Memory::VirtualAlloc;
    use windows_sys::Win32::System::Memory::VirtualFree;

    pub fn alloc(size: usize) -> std::io::Result<*mut u8> {
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
        let ret = unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
        assert!(ret != 0);
    }
}

#[cfg(unix)]
mod unix {
    use std::ptr;

    pub fn alloc(size: usize) -> std::io::Result<*mut u8> {
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
        let ret = unsafe { libc::munmap(ptr.cast::<libc::c_void>(), size) };
        assert!(ret == 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn test_allocation_contents_shared_through_atomic_view() {
        let mut allocation = Allocation::new(4096).unwrap();
        assert!(allocation.iter().all(|&b| b == 0));
        allocation[7..9].copy_from_slice(&[0x12, 0x34]);
        assert_eq!(&allocation[7..9], &[0x12, 0x34]);

        let shared = SharedMem::new(allocation);
        assert_eq!(shared[7].load(Ordering::Relaxed), 0x12);
        assert_eq!(shared[8].fetch_add(1, Ordering::Relaxed), 0x34);
        assert_eq!(shared[8].load(Ordering::Relaxed), 0x35);
        assert_eq!(shared[9].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_allocation_rejects_zero_size() {
        assert!(Allocation::new(0).is_err());
    }

    #[test]
    fn test_alloc_free() -> Result<(), Box<dyn std::error::Error>> {
        unsafe {
            let x = sys::alloc(4096)?;
            sys::free(x, 4096);
            Ok(())
        }
    }
}
