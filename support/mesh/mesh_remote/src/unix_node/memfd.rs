// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for building shared memory buffers via memfd.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::ptr::null_mut;

/// An object with exclusive access to a writable, mapped memfd.
pub struct MemfdBuilder {
    file: Option<File>,
    mapping: *mut u8,
    len: usize,
}

const SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;

impl MemfdBuilder {
    /// Makes a new memfd of the specified size.
    pub fn new(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mapping size cannot be 0",
            ));
        }
        let len = (len + 4095) & !4095;
        // SAFETY: calling as documented.
        let file = unsafe {
            let fd = libc::memfd_create(
                c"mesh".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            );
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            File::from_raw_fd(fd)
        };
        file.set_len(len as u64)?;
        // SAFETY: mapping the fd created above.
        unsafe {
            let mapping = libc::mmap(
                null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            if mapping == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                file: Some(file),
                mapping: mapping.cast(),
                len,
            })
        }
    }

    fn unmap(&mut self) {
        // SAFETY: the mapping being unmapped is exclusively owned by this
        // object.
        if self.len > 0 && unsafe { libc::munmap(self.mapping.cast(), self.len) } < 0 {
            panic!("unmap failure: {}", io::Error::last_os_error());
        }
        self.len = 0;
    }

    pub fn seal(mut self) -> io::Result<File> {
        // Unmap first or the seal operation will fail.
        self.unmap();
        let file = self.file.take().unwrap();
        // SAFETY: calling as documented.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
}

impl Drop for MemfdBuilder {
    fn drop(&mut self) {
        self.unmap();
    }
}

impl Deref for MemfdBuilder {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: this object has exclusive access to the mapping and its
        // underlying memfd object.
        unsafe { std::slice::from_raw_parts(self.mapping, self.len) }
    }
}

impl DerefMut for MemfdBuilder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this object has exclusive access to the mapping and its
        // underlying memfd object.
        unsafe { std::slice::from_raw_parts_mut(self.mapping, self.len) }
    }
}

/// An object with shared access to a sealed, mapped memfd that allows no
/// further writes or resizes.
pub struct SealedMemfd {
    mapping: *const u8,
    len: usize,
}

impl SealedMemfd {
    /// Maps a memfd after ensuring that it is appropriately sealed.
    pub fn new(file: File) -> io::Result<Self> {
        // Make sure the file has been sealed to write access since we will
        // be accessing it as an immutable slice.
        // SAFETY: file is guaranteed to be a valid fd.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 {
            return Err(io::Error::last_os_error());
        }
        if seals & SEALS != SEALS {
            return Err(io::Error::new(io::ErrorKind::Other, "memfd is not sealed"));
        }
        let len = file.metadata()?.len() as usize;
        // SAFETY: mapping a valid fd.
        unsafe {
            let mapping = libc::mmap(
                null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE, // MAP_SHARED is blocked by the kernel for sealed memfds
                file.as_raw_fd(),
                0,
            );
            if mapping == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                mapping: mapping.cast_const().cast(),
                len,
            })
        }
    }

    fn unmap(&mut self) {
        // SAFETY: this object has exclusive access to this VA range.
        if self.len > 0 && unsafe { libc::munmap(self.mapping.cast_mut().cast(), self.len) } < 0 {
            panic!("unmap failure: {}", io::Error::last_os_error());
        }
        self.len = 0;
    }
}

impl Deref for SealedMemfd {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: this VA range is valid for read. The memfd seals ensure that
        // the data cannot mutate from under us.
        unsafe { std::slice::from_raw_parts(self.mapping, self.len) }
    }
}

impl Drop for SealedMemfd {
    fn drop(&mut self) {
        self.unmap();
    }
}
