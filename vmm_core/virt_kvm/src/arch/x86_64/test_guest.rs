// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest RAM for tests that run a real-mode guest in a KVM VM.

use kvm::Partition;

/// Guest RAM at GPA 0, in memory slot 0 of a VM.
pub(super) struct GuestRam<'a> {
    partition: &'a Partition,
    ptr: *mut u8,
    len: usize,
}

impl<'a> GuestRam<'a> {
    pub fn new(partition: &'a Partition, len: usize) -> Self {
        // SAFETY: creating a new anonymous mapping.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let ptr = ptr.cast::<u8>();
        // SAFETY: the mapping stays valid until `drop` clears the slot.
        unsafe {
            partition
                .set_user_memory_region(0, ptr, len, 0, false)
                .unwrap();
        }
        Self {
            partition,
            ptr,
            len,
        }
    }

    pub fn write(&self, gpa: u64, data: &[u8]) {
        let offset = gpa as usize;
        assert!(offset + data.len() <= self.len);
        // SAFETY: the range is within the mapping, and the guest is not
        // running yet.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data.len()) };
    }
}

impl Drop for GuestRam<'_> {
    fn drop(&mut self) {
        // SAFETY: clearing the slot, after which KVM no longer references
        // the mapping.
        let cleared = unsafe {
            self.partition
                .set_user_memory_region(0, std::ptr::null_mut(), 0, 0, false)
        };
        // Leak the mapping if KVM might still reference it.
        if cleared.is_ok() {
            // SAFETY: unmapping the mapping created in `new`, which KVM no
            // longer references.
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }
}
