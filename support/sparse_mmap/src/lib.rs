// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory-related abstractions.

// UNSAFETY: Manual pointer manipulation, dealing with mmap, and a signal handler.
#![expect(unsafe_code)]
#![expect(missing_docs)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

pub mod alloc;
mod trycopy_windows_arm64;
mod trycopy_windows_x64;
pub mod unix;
pub mod windows;

pub use sys::AsMappableRef;
pub use sys::Mappable;
pub use sys::MappableRef;
pub use sys::SparseMapping;
pub use sys::alloc_shared_memory;
pub use sys::new_mappable_from_file;

use std::mem::MaybeUninit;
use std::sync::atomic::AtomicU8;
use thiserror::Error;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Must be called before using try_copy on Unix platforms.
pub fn initialize_try_copy() {
    #[cfg(unix)]
    {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| unsafe {
            let err = install_signal_handlers();
            if err != 0 {
                panic!(
                    "could not install signal handlers: {}",
                    std::io::Error::from_raw_os_error(err)
                )
            }
        });
    }
}

unsafe extern "C" {
    #[cfg(unix)]
    fn install_signal_handlers() -> i32;

    fn try_memmove(
        dest: *mut u8,
        src: *const u8,
        length: usize,
        failure: *mut AccessFailure,
    ) -> i32;
    fn try_memset(dest: *mut u8, c: i32, length: usize, failure: *mut AccessFailure) -> i32;
    fn try_cmpxchg8(
        dest: *mut u8,
        expected: &mut u8,
        desired: u8,
        failure: *mut AccessFailure,
    ) -> i32;
    fn try_cmpxchg16(
        dest: *mut u16,
        expected: &mut u16,
        desired: u16,
        failure: *mut AccessFailure,
    ) -> i32;
    fn try_cmpxchg32(
        dest: *mut u32,
        expected: &mut u32,
        desired: u32,
        failure: *mut AccessFailure,
    ) -> i32;
    fn try_cmpxchg64(
        dest: *mut u64,
        expected: &mut u64,
        desired: u64,
        failure: *mut AccessFailure,
    ) -> i32;
    fn try_read8(dest: *mut u8, src: *const u8, failure: *mut AccessFailure) -> i32;
    fn try_read16(dest: *mut u16, src: *const u16, failure: *mut AccessFailure) -> i32;
    fn try_read32(dest: *mut u32, src: *const u32, failure: *mut AccessFailure) -> i32;
    fn try_read64(dest: *mut u64, src: *const u64, failure: *mut AccessFailure) -> i32;
    fn try_write8(dest: *mut u8, value: u8, failure: *mut AccessFailure) -> i32;
    fn try_write16(dest: *mut u16, value: u16, failure: *mut AccessFailure) -> i32;
    fn try_write32(dest: *mut u32, value: u32, failure: *mut AccessFailure) -> i32;
    fn try_write64(dest: *mut u64, value: u64, failure: *mut AccessFailure) -> i32;
}

#[repr(C)]
struct AccessFailure {
    address: *mut u8,
    #[cfg(unix)]
    si_signo: i32,
    #[cfg(unix)]
    si_code: i32,
}

#[derive(Debug, Error)]
#[error("failed to {} memory", if self.is_write { "write" } else { "read" })]
pub struct MemoryError {
    offset: usize,
    is_write: bool,
    #[source]
    source: OsAccessError,
}

#[derive(Debug, Error)]
enum OsAccessError {
    #[cfg(windows)]
    #[error("access violation")]
    AccessViolation,
    #[cfg(unix)]
    #[error("SIGSEGV (si_code = {0:x}")]
    Sigsegv(u32),
    #[cfg(unix)]
    #[error("SIGSEGV (si_code = {0:x}")]
    Sigbus(u32),
}

impl MemoryError {
    fn new(src: Option<*const u8>, dest: *mut u8, len: usize, failure: &AccessFailure) -> Self {
        let (offset, is_write) = if failure.address.is_null() {
            // In the case of a general protection fault (#GP) the provided address is zero.
            (0, src.is_none())
        } else if (dest..dest.wrapping_add(len)).contains(&failure.address) {
            (failure.address as usize - dest as usize, true)
        } else if let Some(src) = src {
            if (src..src.wrapping_add(len)).contains(&failure.address.cast_const()) {
                (failure.address as usize - src as usize, false)
            } else {
                panic!(
                    "invalid failure address: {:p} src: {:p} dest: {:p} len: {:#x}",
                    failure.address, src, dest, len
                );
            }
        } else {
            panic!(
                "invalid failure address: {:p} src: None dest: {:p} len: {:#x}",
                failure.address, dest, len
            );
        };
        #[cfg(windows)]
        let source = OsAccessError::AccessViolation;
        #[cfg(unix)]
        let source = match failure.si_signo {
            libc::SIGSEGV => OsAccessError::Sigsegv(failure.si_code as u32),
            libc::SIGBUS => OsAccessError::Sigbus(failure.si_code as u32),
            _ => {
                panic!(
                    "unexpected signal: {} src: {:?} dest: {:p} len: {:#x}",
                    failure.si_signo, src, dest, len
                );
            }
        };
        Self {
            offset,
            is_write,
            source,
        }
    }

    /// Returns the byte offset into the buffer at which the access violation
    /// occurred.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// Copies `count` elements from `src` to `dest`. `src` and `dest` may overlap.
/// Fails on access violation/SIGSEGV. Note that on case of failure, some of the
/// bytes (even partial elements) may already have been copied.
///
/// This also fails if initialize_try_copy has not been called.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `src` or `dest` is
/// being concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `src` and
/// `dest` are valid, reserved addresses but you do not know if they are mapped
/// with the appropriate protection. For example, this routine is useful if
/// `dest` is a sparse mapping where some pages are mapped with
/// PAGE_NOACCESS/PROT_NONE, and some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_copy<T>(src: *const T, dest: *mut T, count: usize) -> Result<(), MemoryError> {
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller.
    let ret = unsafe {
        try_memmove(
            dest.cast::<u8>(),
            src.cast::<u8>(),
            count * size_of::<T>(),
            failure.as_mut_ptr(),
        )
    };
    match ret {
        0 => Ok(()),
        _ => Err(MemoryError::new(
            Some(src.cast()),
            dest.cast(),
            count,
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        )),
    }
}

/// Writes `count` bytes of the value `val` to `dest`. Fails on access
/// violation/SIGSEGV. Note that on case of failure, some of the bytes (even
/// partial elements) may already have been written.
///
/// This also fails if initialize_try_copy has not been called.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `dest` is being
/// concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `dest` is
/// valid, reserved addresses but you do not know if they are mapped with the
/// appropriate protection. For example, this routine is useful if `dest` is a
/// sparse mapping where some pages are mapped with PAGE_NOACCESS/PROT_NONE, and
/// some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_write_bytes<T>(dest: *mut T, val: u8, count: usize) -> Result<(), MemoryError> {
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller.
    let ret = unsafe {
        try_memset(
            dest.cast::<u8>(),
            val.into(),
            count * size_of::<T>(),
            failure.as_mut_ptr(),
        )
    };
    match ret {
        0 => Ok(()),
        _ => Err(MemoryError::new(
            None,
            dest.cast(),
            count,
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        )),
    }
}

/// Atomically swaps the value at `dest` with `new` when `*dest` is `current`,
/// using a sequentially-consistent memory ordering.
///
/// Returns `Ok(Ok(new))` if the swap was successful, `Ok(Err(*dest))` if the
/// swap failed, or `Err(MemoryError::AccessViolation)` if the swap could not be
/// attempted due to an access violation.
///
/// Panics if the size is not 1, 2, 4, or 8 bytes.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `dest` is being
/// concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `dest` is
/// valid, reserved addresses but you do not know if they are mapped with the
/// appropriate protection. For example, this routine is useful if `dest` is a
/// sparse mapping where some pages are mapped with PAGE_NOACCESS/PROT_NONE, and
/// some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_compare_exchange<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
    dest: *mut T,
    mut current: T,
    new: T,
) -> Result<Result<T, T>, MemoryError> {
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_cmpxchg8(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u8>(&mut current),
                std::mem::transmute_copy::<T, u8>(&new),
                failure.as_mut_ptr(),
            ),
            2 => try_cmpxchg16(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u16>(&mut current),
                std::mem::transmute_copy::<T, u16>(&new),
                failure.as_mut_ptr(),
            ),
            4 => try_cmpxchg32(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u32>(&mut current),
                std::mem::transmute_copy::<T, u32>(&new),
                failure.as_mut_ptr(),
            ),
            8 => try_cmpxchg64(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u64>(&mut current),
                std::mem::transmute_copy::<T, u64>(&new),
                failure.as_mut_ptr(),
            ),
            _ => panic!("unsupported size"),
        }
    };
    match ret {
        n if n > 0 => Ok(Ok(new)),
        0 => Ok(Err(current)),
        _ => Err(MemoryError::new(
            None,
            dest.cast(),
            size_of::<T>(),
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        )),
    }
}

/// Atomically swaps the value at `dest` with `new` when `*dest` is `current`,
/// using a sequentially-consistent memory ordering.
///
/// Returns `Ok(true)` if the swap was successful, `Ok(false)` if the swap
/// failed (after updating `current`), or `Err(MemoryError::AccessViolation)` if
/// the swap could not be attempted due to an access violation.
///
/// Panics if `current` and `new` are not the same size or that size is not
/// 1, 2, 4, or 8 bytes.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `dest` is being
/// concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `dest` is
/// valid, reserved addresses but you do not know if they are mapped with the
/// appropriate protection. For example, this routine is useful if `dest` is a
/// sparse mapping where some pages are mapped with PAGE_NOACCESS/PROT_NONE, and
/// some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_compare_exchange_ref<
    T: IntoBytes + FromBytes + Immutable + KnownLayout + ?Sized,
>(
    dest: *mut u8,
    current: &mut T,
    new: &T,
) -> Result<bool, MemoryError> {
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match (size_of_val(current), size_of_val(new)) {
            (1, 1) => try_cmpxchg8(
                dest,
                &mut *current.as_mut_bytes().as_mut_ptr(),
                new.as_bytes()[0],
                failure.as_mut_ptr(),
            ),
            (2, 2) => try_cmpxchg16(
                dest.cast(),
                &mut *current.as_mut_bytes().as_mut_ptr().cast(),
                u16::from_ne_bytes(new.as_bytes().try_into().unwrap()),
                failure.as_mut_ptr(),
            ),
            (4, 4) => try_cmpxchg32(
                dest.cast(),
                &mut *current.as_mut_bytes().as_mut_ptr().cast(),
                u32::from_ne_bytes(new.as_bytes().try_into().unwrap()),
                failure.as_mut_ptr(),
            ),
            (8, 8) => try_cmpxchg64(
                dest.cast(),
                &mut *current.as_mut_bytes().as_mut_ptr().cast(),
                u64::from_ne_bytes(new.as_bytes().try_into().unwrap()),
                failure.as_mut_ptr(),
            ),
            _ => panic!("unsupported or mismatched size"),
        }
    };
    if ret < 0 {
        return Err(MemoryError::new(
            None,
            dest.cast(),
            size_of_val(current),
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        ));
    }
    Ok(ret > 0)
}

/// Reads the value at `src` treating the pointer as a volatile access.
///
/// Returns `Ok(T)` if the read was successful, or `Err(MemoryError)` if the
/// read was unsuccessful.
///
/// Panics if the size is not 1, 2, 4, or 8 bytes.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `src` is being
/// concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `src` is
/// valid, reserved addresses but you do not know if they are mapped with the
/// appropriate protection. For example, this routine is useful if `src` is a
/// sparse mapping where some pages are mapped with PAGE_NOACCESS/PROT_NONE, and
/// some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_read_volatile<T: FromBytes + Immutable + KnownLayout>(
    src: *const T,
) -> Result<T, MemoryError> {
    let mut dest = MaybeUninit::<T>::uninit();
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_read8(dest.as_mut_ptr().cast(), src.cast(), failure.as_mut_ptr()),
            2 => try_read16(dest.as_mut_ptr().cast(), src.cast(), failure.as_mut_ptr()),
            4 => try_read32(dest.as_mut_ptr().cast(), src.cast(), failure.as_mut_ptr()),
            8 => try_read64(dest.as_mut_ptr().cast(), src.cast(), failure.as_mut_ptr()),
            _ => panic!("unsupported size"),
        }
    };
    match ret {
        0 => {
            // SAFETY: dest was fully initialized by try_read.
            Ok(unsafe { dest.assume_init() })
        }
        _ => Err(MemoryError::new(
            Some(src.cast()),
            dest.as_mut_ptr().cast(),
            size_of::<T>(),
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        )),
    }
}

/// Writes `value` at `dest` treating the pointer as a volatile access.
///
/// Returns `Ok(())` if the write was successful, or `Err(MemoryError)` if the
/// write was unsuccessful.
///
/// Panics if the size is not 1, 2, 4, or 8 bytes.
///
/// # Safety
///
/// This routine is safe to use if the memory pointed to by `dest` is being
/// concurrently mutated.
///
/// WARNING: This routine should only be used when you know that `dest` is
/// valid, reserved addresses but you do not know if they are mapped with the
/// appropriate protection. For example, this routine is useful if `dest` is a
/// sparse mapping where some pages are mapped with PAGE_NOACCESS/PROT_NONE, and
/// some are mapped with PAGE_READWRITE/PROT_WRITE.
pub unsafe fn try_write_volatile<T: IntoBytes + Immutable + KnownLayout>(
    dest: *mut T,
    value: &T,
) -> Result<(), MemoryError> {
    let mut failure = MaybeUninit::uninit();
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_write8(
                dest.cast(),
                std::mem::transmute_copy(value),
                failure.as_mut_ptr(),
            ),
            2 => try_write16(
                dest.cast(),
                std::mem::transmute_copy(value),
                failure.as_mut_ptr(),
            ),
            4 => try_write32(
                dest.cast(),
                std::mem::transmute_copy(value),
                failure.as_mut_ptr(),
            ),
            8 => try_write64(
                dest.cast(),
                std::mem::transmute_copy(value),
                failure.as_mut_ptr(),
            ),
            _ => panic!("unsupported size"),
        }
    };
    match ret {
        0 => Ok(()),
        _ => Err(MemoryError::new(
            None,
            dest.cast(),
            size_of::<T>(),
            // SAFETY: failure is initialized in the failure path.
            unsafe { failure.assume_init_ref() },
        )),
    }
}

#[derive(Debug, Error)]
pub enum SparseMappingError {
    #[error("out of bounds")]
    OutOfBounds,
    #[error(transparent)]
    Memory(MemoryError),
}

impl SparseMapping {
    /// Gets the supported page size for sparse mappings.
    pub fn page_size() -> usize {
        sys::page_size()
    }

    fn check(&self, offset: usize, len: usize) -> Result<(), SparseMappingError> {
        if self.len() < offset || self.len() - offset < len {
            return Err(SparseMappingError::OutOfBounds);
        }
        Ok(())
    }

    /// Reads a type `T` from `offset` in the sparse mapping using a single read instruction.
    ///
    /// Panics if `T` is not 1, 2, 4, or 8 bytes in size.
    pub fn read_volatile<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> Result<T, SparseMappingError> {
        assert!(self.is_local(), "cannot read from remote mappings");

        self.check(offset, size_of::<T>())?;
        // SAFETY: the bounds have been checked above.
        unsafe { try_read_volatile(self.as_ptr().cast::<u8>().add(offset).cast()) }
            .map_err(SparseMappingError::Memory)
    }

    /// Writes a type `T` at `offset` in the sparse mapping using a single write instruciton.
    ///
    /// Panics if `T` is not 1, 2, 4, or 8 bytes in size.
    pub fn write_volatile<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
        value: &T,
    ) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot write to remote mappings");

        self.check(offset, size_of::<T>())?;
        // SAFETY: the bounds have been checked above.
        unsafe { try_write_volatile(self.as_ptr().cast::<u8>().add(offset).cast(), value) }
            .map_err(SparseMappingError::Memory)
    }

    /// Tries to write into the sparse mapping.
    pub fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot write to remote mappings");

        self.check(offset, data.len())?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let dest = self.as_ptr().cast::<u8>().add(offset);
            try_copy(data.as_ptr(), dest, data.len()).map_err(SparseMappingError::Memory)
        }
    }

    /// Tries to read from the sparse mapping.
    pub fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot read from remote mappings");

        self.check(offset, data.len())?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let src = (self.as_ptr() as *const u8).add(offset);
            try_copy(src, data.as_mut_ptr(), data.len()).map_err(SparseMappingError::Memory)
        }
    }

    /// Tries to read a type `T` from `offset`.
    pub fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> Result<T, SparseMappingError> {
        if matches!(size_of::<T>(), 1 | 2 | 4 | 8) {
            self.read_volatile(offset)
        } else {
            let mut obj = MaybeUninit::<T>::uninit();
            // SAFETY: `obj` is a valid target for writes.
            unsafe {
                self.read_at(
                    offset,
                    std::slice::from_raw_parts_mut(obj.as_mut_ptr().cast::<u8>(), size_of::<T>()),
                )?;
            }
            // SAFETY: `obj` was fully initialized by `read_at`.
            Ok(unsafe { obj.assume_init() })
        }
    }

    /// Tries to fill a region of the sparse mapping with `val`.
    pub fn fill_at(&self, offset: usize, val: u8, len: usize) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot fill remote mappings");

        self.check(offset, len)?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let dest = self.as_ptr().cast::<u8>().add(offset);
            try_write_bytes(dest, val, len).map_err(SparseMappingError::Memory)
        }
    }

    /// Gets a slice for accessing the mapped data directly.
    ///
    /// This is safe from a Rust memory model perspective, since the underlying
    /// VA is either mapped and is owned in a shared state by this object (in
    /// which case &[AtomicU8] access from multiple threads is fine), or the VA
    /// is not mapped but is reserved and so will not be mapped by another Rust
    /// object.
    ///
    /// In the latter case, actually accessing the data may cause a fault, which
    /// will likely lead to a process crash, so care must nonetheless be taken
    /// when using this method.
    pub fn atomic_slice(&self, start: usize, len: usize) -> &[AtomicU8] {
        assert!(self.len() >= start && self.len() - start >= len);
        // SAFETY: slice is within the mapped range
        unsafe { std::slice::from_raw_parts((self.as_ptr() as *const AtomicU8).add(start), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Debug)]
    enum Primitive {
        Read,
        Write,
        CompareAndSwap,
    }

    #[repr(u32)]
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Size {
        Bit8 = 8,
        Bit16 = 16,
        Bit32 = 32,
        Bit64 = 64,
    }

    fn test_unsafe_primitive(primitive: Primitive, size: Size) {
        // NOTE: this test provides a very basic validation of
        // the compare-and-swap operation, mostly to check that
        // the failures address in returned correctly. See other tests
        // for more.
        let mut dest = !0u64;
        let dest_addr = std::ptr::from_mut(&mut dest).cast::<()>();
        let src = 0x5555_5555_5555_5555u64;
        let src_addr = std::ptr::from_ref(&src).cast::<()>();
        let bad_addr_mut = 0x100 as *mut (); // Within 0..0x1000
        let bad_addr = bad_addr_mut.cast_const();
        let nonsense_addr = !0u64 as *mut ();
        let expected = if size != Size::Bit64 {
            dest.wrapping_shl(size as u32) | src.wrapping_shr(64 - (size as u32))
        } else {
            src
        };
        let mut af = AccessFailure {
            address: nonsense_addr.cast(),
            #[cfg(unix)]
            si_signo: 0,
            #[cfg(unix)]
            si_code: 0,
        };
        let af_addr = &mut af as *mut _;

        let res = unsafe {
            match size {
                Size::Bit8 => match primitive {
                    Primitive::Read => try_read8(dest_addr.cast(), src_addr.cast(), af_addr),
                    Primitive::Write => try_write8(dest_addr.cast(), src as u8, af_addr),
                    Primitive::CompareAndSwap => {
                        1 - try_cmpxchg8(dest_addr.cast(), &mut (dest as u8), src as u8, af_addr)
                    }
                },
                Size::Bit16 => match primitive {
                    Primitive::Read => try_read16(dest_addr.cast(), src_addr.cast(), af_addr),
                    Primitive::Write => try_write16(dest_addr.cast(), src as u16, af_addr),
                    Primitive::CompareAndSwap => {
                        1 - try_cmpxchg16(dest_addr.cast(), &mut (dest as u16), src as u16, af_addr)
                    }
                },
                Size::Bit32 => match primitive {
                    Primitive::Read => try_read32(dest_addr.cast(), src_addr.cast(), af_addr),
                    Primitive::Write => try_write32(dest_addr.cast(), src as u32, af_addr),
                    Primitive::CompareAndSwap => {
                        1 - try_cmpxchg32(dest_addr.cast(), &mut (dest as u32), src as u32, af_addr)
                    }
                },
                Size::Bit64 => match primitive {
                    Primitive::Read => try_read64(dest_addr.cast(), src_addr.cast(), af_addr),
                    Primitive::Write => try_write64(dest_addr.cast(), src, af_addr),
                    Primitive::CompareAndSwap => {
                        1 - try_cmpxchg64(dest_addr.cast(), &mut { dest }, src, af_addr)
                    }
                },
            }
        };
        assert_eq!(
            dest, expected,
            "Expected value must match the result for {primitive:?} and {size:?}"
        );
        assert_eq!(
            res, 0,
            "Success should be returned for {primitive:?} and {size:?}"
        );
        assert_eq!(
            af.address,
            nonsense_addr.cast(),
            "Fault address must not be set for {primitive:?} and {size:?}"
        );

        let res = unsafe {
            match size {
                Size::Bit8 => match primitive {
                    Primitive::Read => try_read8(dest_addr.cast(), bad_addr.cast(), af_addr),
                    Primitive::Write => try_write8(bad_addr_mut.cast(), src as u8, af_addr),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg8(bad_addr_mut.cast(), &mut (dest as u8), src as u8, af_addr)
                    }
                },
                Size::Bit16 => match primitive {
                    Primitive::Read => try_read16(dest_addr.cast(), bad_addr.cast(), af_addr),
                    Primitive::Write => try_write16(bad_addr_mut.cast(), src as u16, af_addr),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg16(bad_addr_mut.cast(), &mut (dest as u16), src as u16, af_addr)
                    }
                },
                Size::Bit32 => match primitive {
                    Primitive::Read => try_read32(dest_addr.cast(), bad_addr.cast(), af_addr),
                    Primitive::Write => try_write32(bad_addr_mut.cast(), src as u32, af_addr),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg32(bad_addr_mut.cast(), &mut (dest as u32), src as u32, af_addr)
                    }
                },
                Size::Bit64 => match primitive {
                    Primitive::Read => try_read64(dest_addr.cast(), bad_addr.cast(), af_addr),
                    Primitive::Write => try_write64(bad_addr_mut.cast(), src, af_addr),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg64(bad_addr_mut.cast(), &mut { dest }, src, af_addr)
                    }
                },
            }
        };
        assert_eq!(
            dest, expected,
            "Fault preserved source and destination for {primitive:?} and {size:?}"
        );
        assert_eq!(
            res, -1,
            "Error code must be returned for {primitive:?} and {size:?}"
        );
        assert_eq!(
            af.address,
            bad_addr_mut.cast(),
            "Fault address must be set for {primitive:?} and {size:?}"
        );
    }

    #[test]
    fn test_unsafe_primitives() {
        initialize_try_copy();

        for primitive in [Primitive::Read, Primitive::Write, Primitive::CompareAndSwap] {
            for size in [Size::Bit8, Size::Bit16, Size::Bit32, Size::Bit64] {
                test_unsafe_primitive(primitive, size);
            }
        }
    }

    static BUF: [u8; 65536] = [0xcc; 65536];

    fn test_with(range_size: usize) {
        let page_size = SparseMapping::page_size();

        let mapping = SparseMapping::new(range_size).unwrap();
        mapping.alloc(page_size, page_size).unwrap();
        let slice = unsafe {
            std::slice::from_raw_parts_mut(mapping.as_ptr().add(page_size).cast::<u8>(), page_size)
        };
        slice.copy_from_slice(&BUF[..page_size]);
        mapping.unmap(page_size, page_size).unwrap();

        mapping.alloc(range_size - page_size, page_size).unwrap();
        let slice = unsafe {
            std::slice::from_raw_parts_mut(
                mapping.as_ptr().add(range_size - page_size).cast::<u8>(),
                page_size,
            )
        };
        slice.copy_from_slice(&BUF[..page_size]);
        mapping.unmap(range_size - page_size, page_size).unwrap();
        drop(mapping);
    }

    #[test]
    fn test_sparse_mapping() {
        test_with(0x100000);
        test_with(0x200000);
        test_with(0x200000 + SparseMapping::page_size());
        test_with(0x40000000);
        test_with(0x40000000 + SparseMapping::page_size());
    }

    #[test]
    fn test_try_copy() {
        initialize_try_copy();

        let mapping = SparseMapping::new(2 * 1024 * 1024).unwrap();
        let page_size = SparseMapping::page_size();
        mapping.alloc(page_size, page_size).unwrap();
        let base = mapping.as_ptr().cast::<u8>();
        unsafe {
            try_copy(BUF.as_ptr(), base, 100).unwrap_err();
            try_copy(BUF.as_ptr(), base.add(page_size), 100).unwrap();
            try_copy(BUF.as_ptr(), base.add(page_size), page_size + 1).unwrap_err();
        }
    }

    #[test]
    fn test_cmpxchg() {
        initialize_try_copy();

        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(page_size * 2).unwrap();
        mapping.alloc(0, page_size).unwrap();
        let base = mapping.as_ptr().cast::<u8>();
        unsafe {
            assert_eq!(try_compare_exchange(base.add(8), 0, 1).unwrap().unwrap(), 1);
            assert_eq!(
                try_compare_exchange(base.add(8), 0, 2)
                    .unwrap()
                    .unwrap_err(),
                1
            );
            assert_eq!(
                try_compare_exchange(base.cast::<u64>().add(1), 1, 2)
                    .unwrap()
                    .unwrap(),
                2
            );
            assert!(try_compare_exchange_ref(base.add(8), &mut [2u8, 0], &[3, 0]).unwrap());
            try_compare_exchange(base.add(page_size), 0, 2).unwrap_err();
        }
    }

    #[test]
    fn test_overlapping_mappings() {
        #![expect(clippy::identity_op)]

        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(0x10 * page_size).unwrap();
        mapping.alloc(0x1 * page_size, 0x4 * page_size).unwrap();
        mapping.alloc(0x1 * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x2 * page_size, 0x3 * page_size).unwrap();
        mapping.alloc(0, 0x10 * page_size).unwrap();
        mapping.alloc(0x8 * page_size, 0x8 * page_size).unwrap();
        mapping.unmap(0xc * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x9 * page_size, 0x4 * page_size).unwrap();
        mapping.unmap(0x3 * page_size, 0xb * page_size).unwrap();

        mapping.alloc(0x5 * page_size, 0x4 * page_size).unwrap();
        mapping.alloc(0x6 * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x6 * page_size, 0x1 * page_size).unwrap();
        mapping.alloc(0x4 * page_size, 0x3 * page_size).unwrap();

        let shmem = alloc_shared_memory(0x4 * page_size).unwrap();
        mapping
            .map_file(0x5 * page_size, 0x4 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x6 * page_size, 0x2 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x6 * page_size, 0x1 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x4 * page_size, 0x3 * page_size, &shmem, 0, true)
            .unwrap();

        drop(mapping);
    }
}
