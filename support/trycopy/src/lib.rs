// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Safe memory operations across trust boundaries with fault recovery.
//!
//! This crate provides memory access primitives (copy, read, write,
//! compare-exchange) that can safely handle access violations without
//! panicking. It's designed for scenarios where you need to access memory that
//! may not be properly mapped or protected, such as guest VM memory in a VMM.
//!
//! # Use Case
//!
//! In virtualization contexts like OpenVMM and OpenHCL, the VMM must access
//! guest memory that may not be fully mapped or may have varying protection
//! levels. Directly accessing such memory can lead to access violations (e.g.,
//! SIGSEGV on Unix or access violations on Windows), which would typically
//! crash the VMM. This crate provides safe abstractions to perform these memory
//! operations while gracefully handling faults.
//!
//! # How It Works
//!
//! The implementation uses a combination of:
//!
//! 1. **Architecture-specific assembly**: Each memory operation is implemented
//!    using inline assembly (for x86_64 and aarch64) with precise control over
//!    which instructions might fault.
//!
//! 2. **Recovery descriptors**: Each faulting code region is annotated with a
//!    `RecoveryDescriptor` stored in a special linker section. These
//!    descriptors map instruction pointer ranges to recovery code.
//!
//! 3. **Signal/exception handlers**: Global handlers for SIGSEGV/SIGBUS (Unix)
//!    or vectored exception handlers (Windows) intercept access violations.
//!    When a fault occurs, the handler searches the recovery descriptor table
//!    for a matching instruction pointer range and, if found, redirects
//!    execution to the recovery code.
//!
//! 4. **Thread-local fault tracking**: The faulting address and fault details
//!    are stored in thread-local storage, allowing the caller to determine
//!    exactly where and why the access failed.
//!
//! # Initialization
//!
//! Before using any operations, you must call [`initialize_try_copy`] once.
//! This installs the necessary signal/exception handlers. Calling it multiple
//! times is safe (only the first call has an effect).
//!
//! # Example
//!
//! ```no_run
//! trycopy::initialize_try_copy();
//!
//! // Attempt to read from potentially unmapped guest memory
//! let guest_ptr = 0x1000 as *const u64;
//! match unsafe { trycopy::try_read_volatile(guest_ptr) } {
//!     Ok(value) => println!("Read value: {:#x}", value),
//!     Err(e) => println!("Access failed at offset {}: {}", e.offset(), e),
//! }
//! ```
//!
//! # Safety Guarantees
//!
//! These operations are safe to use even when:
//! - The memory is being concurrently modified
//! - The memory may not be mapped at all
//! - The memory has incorrect protection attributes
//!
//! However, callers must still ensure:
//! - Pointers are properly aligned for their type (for atomic operations)
//! - The address space is valid and reserved (even if not committed/mapped)
//! - Concurrent access doesn't violate Rust's aliasing rules in safe code
//!
//! # Performance
//!
//! The inline assembly can be inlined by the compiler, ensuring overhead in the
//! success case is comparable to an ordinary relaxed `AtomicU<n>` memory access
//! or `memcpy` call. The fault case is expensive (signal handling, table
//! lookup), but that's expected since faults are exceptional conditions.

// UNSAFETY: all kinds of assembly, signal handling.
#![expect(unsafe_code)]

mod aarch64;
mod memcpy;
mod x86_64;

#[cfg(target_arch = "aarch64")]
use aarch64::*;
#[cfg(target_arch = "x86_64")]
use x86_64::*;

use std::mem::MaybeUninit;
use thiserror::Error;

/// Must be called before using [`try_copy`] or other `try_` functions with a
/// memory buffer that could fault.
pub fn initialize_try_copy() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // SAFETY: calling just once, as required.
        unsafe {
            install_signal_handlers();
        }
    });
}

/// Copies `count` elements from `src` to `dest`, returning an error if an
/// access violation occurs. The source and destination may overlap.
///
/// No guarantees are made about the access width used to perform the copy or
/// the order in which the accesses are made. In the case of failure, some of
/// the bytes (even partial elements) may already have been copied.
///
/// If [`initialize_try_copy`] has not been called and a fault occurs, the
/// process will be terminated according to the platform's default behavior.
///
/// # Safety
/// `src` and `dest` must point to reserved addresses, which may or may not
/// actually be backed. `dest` cannot point to memory that would violate Rust's
/// aliasing rules.
///
/// Note that this creates a bitwise copy of the data, even if `T` is not
/// `Copy`. The caller must ensure that subsequent uses of `src` or `dest` do
/// not cause undefined behavior.
pub unsafe fn try_copy<T>(src: *const T, dest: *mut T, count: usize) -> Result<(), MemoryError> {
    let len = count * size_of::<T>();
    // SAFETY: guaranteed by caller.
    let ret = unsafe { try_memmove(dest.cast::<u8>(), src.cast::<u8>(), len) };
    match ret {
        Ok(()) => Ok(()),
        Err(Fault) => {
            cold_path();
            Err(MemoryError::from_last_failure(
                Some(src.cast()),
                dest.cast(),
                len,
            ))
        }
    }
}

/// Sets `count * size_of::<T>()` bytes of memory at `dest` to the byte value
/// `val`, returning an error if an access violation occurs.
///
/// No guarantees are made about the access width used to perform the set or the
/// order in which the accesses are made. In the case of failure, some of the
/// bytes may already have been set.
///
/// If [`initialize_try_copy`] has not been called and a fault occurs, the
/// process will be terminated according to the platform's default behavior.
///
/// # Safety
/// `dest` must point to reserved addresses, which may or may not be backed.
/// `dest` cannot point to memory that would violate Rust's aliasing rules.
///
/// Note that if the written bytes are not a valid representation of `T`,
/// subsequent uses of the memory may be undefined behavior.
pub unsafe fn try_write_bytes<T>(dest: *mut T, val: u8, count: usize) -> Result<(), MemoryError> {
    let len = count * size_of::<T>();
    // SAFETY: guaranteed by caller.
    let ret = unsafe { try_memset(dest.cast::<u8>(), val, len) };
    match ret {
        Ok(()) => Ok(()),
        Err(Fault) => {
            cold_path();
            Err(MemoryError::from_last_failure(None, dest.cast(), len))
        }
    }
}

/// Atomically swaps the value at `dest` with `new` when `*dest` is `current`,
/// using a sequentially-consistent memory ordering.
///
/// Returns `Ok(Ok(new))` if the swap was successful, `Ok(Err(*dest))` if the
/// swap failed, or `Err(MemoryError::AccessViolation)` if the swap could not be
/// attempted due to an access violation.
///
/// Fails at compile time if the size is not 1, 2, 4, or 8 bytes, or if the type
/// is under-aligned.
///
/// # Safety
/// `dest` must point to a reserved address, which may or may not be backed.
/// `dest` cannot point to memory that would violate Rust's aliasing rules.
pub unsafe fn try_compare_exchange<T: Copy>(
    dest: *mut T,
    mut current: T,
    new: T,
) -> Result<Result<T, T>, MemoryError> {
    const {
        assert!(matches!(size_of::<T>(), 1 | 2 | 4 | 8));
        // This `T` must be at least as aligned as the primitive type's natural
        // alignment (which is its size).
        assert!(align_of::<T>() >= size_of::<T>());
    };
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_cmpxchg8(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u8>(&mut current),
                std::mem::transmute_copy::<T, u8>(&new),
            ),
            2 => try_cmpxchg16(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u16>(&mut current),
                std::mem::transmute_copy::<T, u16>(&new),
            ),
            4 => try_cmpxchg32(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u32>(&mut current),
                std::mem::transmute_copy::<T, u32>(&new),
            ),
            8 => try_cmpxchg64(
                dest.cast(),
                std::mem::transmute::<&mut T, &mut u64>(&mut current),
                std::mem::transmute_copy::<T, u64>(&new),
            ),
            _ => unreachable!(),
        }
    };
    match ret {
        Ok(true) => Ok(Ok(new)),
        Ok(false) => Ok(Err(current)),
        Err(Fault) => {
            cold_path();
            Err(MemoryError::from_last_failure(
                None,
                dest.cast(),
                size_of::<T>(),
            ))
        }
    }
}

/// Reads the value at `src` using one or more read instructions, failing if an
/// access violation occurs.
///
/// If `T` is 1, 2, 4, or 8 bytes in size, then exactly one read instruction is
/// used.
///
/// Returns `Ok(T)` if the read was successful, or `Err(MemoryError)` if the
/// read was unsuccessful.
///
/// # Safety
/// `src` must point to a reserved address, which may or may not be backed.
///
/// Note that this creates a bitwise copy of the data, even if `T` is not
/// `Copy`. The caller must ensure that subsequent uses of the returned value
/// do not cause undefined behavior.
pub unsafe fn try_read_volatile<T>(src: *const T) -> Result<T, MemoryError> {
    let mut dest = MaybeUninit::<T>::uninit();
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_read8(src.cast()).map(|v| {
                dest.write(std::mem::transmute_copy(&v));
            }),
            2 => try_read16(src.cast()).map(|v| {
                dest.write(std::mem::transmute_copy(&v));
            }),
            4 => try_read32(src.cast()).map(|v| {
                dest.write(std::mem::transmute_copy(&v));
            }),
            8 => try_read64(src.cast()).map(|v| {
                dest.write(std::mem::transmute_copy(&v));
            }),
            _ => try_memmove(dest.as_mut_ptr().cast(), src.cast::<u8>(), size_of::<T>()),
        }
    };
    match ret {
        Ok(()) => {
            // SAFETY: dest was fully initialized by try_read.
            Ok(unsafe { dest.assume_init() })
        }
        Err(Fault) => {
            cold_path();
            Err(MemoryError::from_last_failure(
                Some(src.cast()),
                dest.as_mut_ptr().cast(),
                size_of::<T>(),
            ))
        }
    }
}

/// Writes `value` at `dest` using one or more write instructions, failing if an
/// access violation occurs.
///
/// If `T` is 1, 2, 4, or 8 bytes in size, then exactly one write instruction is
/// used.
///
/// Returns `Ok(())` if the write was successful, or `Err(MemoryError)` if the
/// write was unsuccessful.
///
/// # Safety
/// `dest` must point to a reserved address, which may or may not be backed.
/// `dest` cannot point to memory that would violate Rust's aliasing rules.
///
/// Note that this creates a bitwise copy of the data, even if `T` is not
/// `Copy`. The caller must ensure that subsequent uses of `dest` do not
/// cause undefined behavior.
pub unsafe fn try_write_volatile<T>(dest: *mut T, value: &T) -> Result<(), MemoryError> {
    // SAFETY: guaranteed by caller
    let ret = unsafe {
        match size_of::<T>() {
            1 => try_write8(dest.cast(), std::mem::transmute_copy(value)),
            2 => try_write16(dest.cast(), std::mem::transmute_copy(value)),
            4 => try_write32(dest.cast(), std::mem::transmute_copy(value)),
            8 => try_write64(dest.cast(), std::mem::transmute_copy(value)),
            _ => try_memmove(
                dest.cast(),
                std::ptr::from_ref(value).cast(),
                size_of::<T>(),
            ),
        }
    };
    match ret {
        Ok(()) => Ok(()),
        Err(Fault) => {
            cold_path();
            Err(MemoryError::from_last_failure(
                None,
                dest.cast(),
                size_of::<T>(),
            ))
        }
    }
}

/// Error returned when a memory access fails.
#[derive(Debug, Error)]
#[error("failed to {} memory", if self.is_write { "write" } else { "read" })]
pub struct MemoryError {
    offset: usize,
    is_write: bool,
    #[source]
    source: OsAccessError,
}

#[cfg(windows)]
#[derive(Debug, Error)]
enum OsAccessError {
    #[error("access violation")]
    AccessViolation,
}

#[cfg(unix)]
#[derive(Debug, Error)]
struct OsAccessError {
    signal: i32,
    si_code: u32,
    /// The x86_64 exception number (trapno) of the underlying fault.
    #[cfg(target_arch = "x86_64")]
    exception: u8,
    /// The aarch64 Exception Syndrome Register (ESR) value of the underlying
    /// fault.
    #[cfg(target_arch = "aarch64")]
    esr: Option<u64>,
}

#[cfg(unix)]
impl std::fmt::Display for OsAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let signal = match self.signal {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            _ => "unknown signal",
        };
        write!(f, "{} (si_code: {:#x})", signal, self.si_code)?;
        #[cfg(target_arch = "x86_64")]
        write!(f, " (exception: {})", self.exception)?;
        #[cfg(target_arch = "aarch64")]
        if let Some(esr) = self.esr {
            write!(f, " (ESR: {:#x})", esr)?;
        }
        Ok(())
    }
}

impl MemoryError {
    fn from_last_failure(src: Option<*const u8>, dest: *mut u8, len: usize) -> Self {
        let failure = LAST_ACCESS_FAILURE.get();
        let (offset, is_write) = if failure.address.is_null() {
            // In the case of a general protection fault (#GP) the provided
            // address is zero.
            //
            // TODO: get the failure offset from the routine that actually
            // faulted rather than relying on the kernel.
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
        let source = OsAccessError {
            signal: failure.si_signo,
            si_code: failure.si_code,
            #[cfg(target_arch = "x86_64")]
            exception: failure.trapno,
            #[cfg(target_arch = "aarch64")]
            esr: failure.esr,
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

#[derive(Debug)]
struct Fault;

#[derive(Copy, Clone)]
struct AccessFailure {
    address: *mut u8,
    #[cfg(unix)]
    si_signo: i32,
    #[cfg(unix)]
    si_code: u32,
    #[cfg(all(unix, target_arch = "x86_64"))]
    trapno: u8,
    #[cfg(all(unix, target_arch = "aarch64"))]
    esr: Option<u64>,
}

impl AccessFailure {
    const fn empty() -> Self {
        Self {
            address: std::ptr::null_mut(),
            #[cfg(unix)]
            si_signo: 0,
            #[cfg(unix)]
            si_code: 0,
            #[cfg(all(unix, target_arch = "x86_64"))]
            trapno: 0,
            #[cfg(all(unix, target_arch = "aarch64"))]
            esr: None,
        }
    }
}

thread_local! {
    static LAST_ACCESS_FAILURE: std::cell::Cell<AccessFailure> = const {
        std::cell::Cell::new(AccessFailure::empty())
    };
}

// FUTURE: replace with [`core::hint::cold_path`] when stabilized.
#[cold]
fn cold_path() {}

#[cfg(target_os = "linux")]
type Context = libc::mcontext_t;
#[cfg(target_os = "macos")]
type Context = libc::__darwin_mcontext64;
#[cfg(windows)]
type Context = windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

/// # Safety
/// This function installs global signal handlers. It must only be called once.
#[cfg(unix)]
unsafe fn install_signal_handlers() {
    fn handle_signal(sig: i32, info: &libc::siginfo_t, ucontext: &mut libc::ucontext_t) {
        #[cfg(target_os = "linux")]
        let ctx = &mut ucontext.uc_mcontext;

        #[cfg(target_os = "macos")]
        // SAFETY: mcontext is always valid.
        let ctx = unsafe { &mut *ucontext.uc_mcontext };

        let address = if sig == libc::SIGSEGV || sig == libc::SIGBUS {
            // SAFETY: si_addr is always valid to read for SIGSEGV and SIGBUS.
            unsafe { info.si_addr().cast() }
        } else {
            // No address available.
            std::ptr::null_mut()
        };

        let failure = AccessFailure {
            address,
            si_signo: sig,
            si_code: info.si_code as u32,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            trapno: ctx.gregs[libc::REG_TRAPNO as usize] as u8,
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            esr: {
                const ESR_MAGIC: u32 = 0x45535201;
                aarch64_extended_context::<u64>(ctx, ESR_MAGIC).copied()
            },
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            esr: Some(ctx.__es.__esr.into()),
        };

        let recovered = recover(ctx, failure);
        if !recovered {
            std::process::abort();
        }
    }

    // SAFETY: installing signal handlers as documented.
    unsafe {
        let act = libc::sigaction {
            sa_sigaction: handle_signal as usize,
            sa_flags: libc::SA_SIGINFO,
            ..core::mem::zeroed()
        };
        for signal in [
            libc::SIGSEGV,
            libc::SIGBUS,
            // SIGILL is used on Linux/aarch64 to signal certain memory access
            // faults from a hypervisor.
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            libc::SIGILL,
        ] {
            // TODO: chain to previous handler. Doing so safely and correctly
            // might require running this code before `main`, via a constructor.
            libc::sigaction(signal, &act, std::ptr::null_mut());
        }
    }
}

/// Finds extended data in the mcontext for Linux/aarch64.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn aarch64_extended_context<T>(ctx: &libc::mcontext_t, magic: u32) -> Option<&T>
where
    T: zerocopy::FromBytes + zerocopy::KnownLayout + zerocopy::Immutable,
{
    use zerocopy::FromBytes;

    #[derive(FromBytes, Debug)]
    #[repr(C)]
    struct Header {
        magic: u32,
        size: u32,
    }

    // SAFETY: This is the `__reserved` field, which should be pub but...
    // <https://github.com/rust-lang/libc/pull/4823>.
    let data = unsafe { &*(&raw const ctx.pstate).add(2).cast::<[u8; 4096]>() };
    let mut data = data.as_ref();

    loop {
        let (header, _) = Header::read_from_prefix(data).ok()?;
        if header.size == 0 {
            break None;
        }
        let (head, rest) = data.split_at(header.size as usize);
        let this_data = &head[size_of::<Header>()..];
        data = rest;
        if header.magic == magic {
            break T::ref_from_bytes(this_data).ok();
        }
    }
}

/// # Safety
/// This function installs global exception handlers. It must only be called once.
#[cfg(windows)]
unsafe fn install_signal_handlers() {
    use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
    use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
    use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_EXECUTION;
    use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_SEARCH;
    use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS;

    extern "system" fn exception_handler(pointers_ptr: *mut EXCEPTION_POINTERS) -> i32 {
        let (pointers, record, context);
        // SAFETY: pointers and its fields are always valid.
        unsafe {
            pointers = &*pointers_ptr;
            record = &*pointers.ExceptionRecord;
            context = &mut *pointers.ContextRecord;
        }
        if record.ExceptionCode != EXCEPTION_ACCESS_VIOLATION {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let failure = AccessFailure {
            address: record.ExceptionInformation[1] as *mut u8,
        };
        let recovered = recover(context, failure);
        if recovered {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }

    // SAFETY: installing exception handler as documented.
    let handle = unsafe { AddVectoredExceptionHandler(1, Some(exception_handler)) };
    if handle.is_null() {
        panic!("could not install vectored exception handler");
    }
}

#[repr(C)]
struct RecoveryDescriptor {
    /// Start of the faulting code region (relative to the address of this
    /// field).
    start: i32,
    /// End of the faulting code region (relative to the address of this field).
    end: i32,
    /// Recovery address (relative to the address of this field). If zero,
    /// then the instruction pointer will be set to `end` and the result
    /// register will be set to -1.
    recover: i32,
}

/// Returns the recovery descriptor table, found by linker-defined symbols
/// marking the start and end of the section.
#[cfg(unix)]
fn recovery_table() -> &'static [RecoveryDescriptor] {
    // SAFETY: the linker automatically defines these symbols when the section
    // is non-empty.
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        #[link_name = "__start_try_copy"]
        static START_TRY_COPY: [RecoveryDescriptor; 0];
        #[link_name = "__stop_try_copy"]
        static STOP_TRY_COPY: [RecoveryDescriptor; 0];
    }

    // SAFETY: the linker automatically defines these symbols when the section
    // is non-empty.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        // The linker on macOS uses a special naming scheme for section symbols.
        #[link_name = "\x01section$start$__TEXT$__try_copy"]
        static START_TRY_COPY: [RecoveryDescriptor; 0];
        #[link_name = "\x01section$end$__TEXT$__try_copy"]
        static STOP_TRY_COPY: [RecoveryDescriptor; 0];
    }

    // Ensure the section exists even if there no recovery descriptors get
    // generated.
    //
    // SAFETY: just a no-op asm block to force the section to be created.
    unsafe {
        core::arch::asm!(concat!(
            ".pushsection ",
            crate::recovery_section!(),
            "\n",
            ".popsection"
        ))
    };

    // SAFETY: accessing the trycopy section as defined above.
    unsafe {
        std::slice::from_raw_parts(
            START_TRY_COPY.as_ptr(),
            STOP_TRY_COPY
                .as_ptr()
                .offset_from_unsigned(START_TRY_COPY.as_ptr()),
        )
    }
}

/// Returns the recovery descriptor table, found by finding the .section via the
/// PE headers.
///
/// The more typical way to do this on Windows is to use the grouping feature of
/// the linker to create symbols marking the start and end of the section, via
/// something like `.trycopy$a` and `.trycopy$z`, with the elements in between
/// in `.trycopy$b`.
///
/// However, Rust/LLVM inline asm (but not global asm) seems to drop the '$',
/// so this doesn't work. So, we use a different technique.
#[cfg(windows)]
fn recovery_table() -> &'static [RecoveryDescriptor] {
    /// Find a PE section by name.
    fn find_section(name: [u8; 8]) -> Option<(*const u8, usize)> {
        use windows_sys::Win32::System::Diagnostics::Debug::IMAGE_NT_HEADERS64;
        use windows_sys::Win32::System::Diagnostics::Debug::IMAGE_SECTION_HEADER;
        use windows_sys::Win32::System::SystemServices::IMAGE_DOS_HEADER;

        unsafe extern "C" {
            safe static __ImageBase: IMAGE_DOS_HEADER;
        }

        let dos_header = &__ImageBase;
        let base_ptr = &raw const __ImageBase;
        // SAFETY: the current module must have valid PE headers.
        let pe = unsafe {
            &*base_ptr
                .byte_add(dos_header.e_lfanew as usize)
                .cast::<IMAGE_NT_HEADERS64>()
        };
        let number_of_sections: usize = pe.FileHeader.NumberOfSections.into();

        // SAFETY: the section table is laid out in memory according to the PE format.
        let sections = unsafe {
            let base = (&raw const pe.OptionalHeader)
                .byte_add(pe.FileHeader.SizeOfOptionalHeader.into())
                .cast::<IMAGE_SECTION_HEADER>();
            std::slice::from_raw_parts(base, number_of_sections)
        };

        sections.iter().find_map(|section| {
            (section.Name == name).then_some({
                // SAFETY: section data is valid according to the PE format.
                unsafe {
                    (
                        base_ptr.byte_add(section.VirtualAddress as usize).cast(),
                        section.Misc.VirtualSize as usize,
                    )
                }
            })
        })
    }

    let Some((start, len)) = find_section(*b".trycopy") else {
        // No recovery descriptors.
        return &[];
    };
    assert_eq!(len % size_of::<RecoveryDescriptor>(), 0);
    // SAFETY: this section is made up solely of RecoveryDescriptor entries.
    unsafe {
        std::slice::from_raw_parts(
            start.cast::<RecoveryDescriptor>(),
            len / size_of::<RecoveryDescriptor>(),
        )
    }
}

fn recover(context: &mut Context, failure: AccessFailure) -> bool {
    let ip = get_context_ip(context);

    // Search for a matching recovery descriptor.
    for r in recovery_table() {
        let reloc = |addr: &i32| -> usize {
            core::ptr::from_ref(addr)
                .addr()
                .wrapping_add_signed(*addr as isize)
        };
        let end = reloc(&r.end);
        if ip >= reloc(&r.start) && ip < end {
            // Write the recovery info.
            //
            // Note that this is not generally guaranteed to be async signal safe,
            // but in this case we know the thread is running in a recovery region,
            // so it is fine.
            LAST_ACCESS_FAILURE.set(failure);

            // Adjust the instruction pointer to the recovery address and write
            // the failure code.
            let (ip, result) = if r.recover == 0 {
                (end, Some(-1))
            } else {
                (reloc(&r.recover), None)
            };

            set_context_ip_and_result(context, ip, result);
            return true;
        }
    }
    false
}

#[cfg(target_os = "linux")]
macro_rules! recovery_section {
    () => {
        // a = allocate, R = retain: don't discard on linking.
        "try_copy,\"aR\""
    };
}

#[cfg(target_os = "windows")]
macro_rules! recovery_section {
    () => {
        // d = data, r = read-only
        ".trycopy,\"dr\""
    };
}

#[cfg(target_os = "macos")]
macro_rules! recovery_section {
    () => {
        // __TEXT = read-only segment, regular = regular section, no_dead_strip
        // = don't discard on linking.
        "__TEXT,__try_copy,regular,no_dead_strip"
    };
}

/// Used within an asm block. Inserts a [`RecoveryDescriptor`] into the binary.
/// The first and second parameters are labels marking the start and end of the
/// code region to recover from--any access faults within that region will be
/// recovered by jumping to the recovery label given as the third parameter.
///
/// If the third parameter is the special value ".", then instead of jumping to
/// the recovery label, the instruction pointer will be set to the end of the
/// code region, and the result register (rcx on x86_64, x0 on aarch64) will be
/// set to -1 to indicate failure.
///
/// FUTURE: remove this extra result register behavior once Rust supports
/// `label` with inline asm blocks that have outputs.
macro_rules! recovery_descriptor {
    ($start:tt, $stop:tt, $recover:tt) => {
        concat!(
            ".pushsection ",
            crate::recovery_section!(),
            "\n",
            ".balign 4\n",
            ".long ",
            $start,
            " - .\n",
            ".long ",
            $stop,
            " - .\n",
            ".long ",
            $recover,
            " - .\n",
            ".popsection"
        )
    };
}

use recovery_descriptor;
use recovery_section;

#[cfg(test)]
mod tests {
    #![expect(clippy::undocumented_unsafe_blocks)]

    use crate::AccessFailure;
    use crate::LAST_ACCESS_FAILURE;
    use crate::initialize_try_copy;
    use crate::try_cmpxchg8;
    use crate::try_cmpxchg16;
    use crate::try_cmpxchg32;
    use crate::try_cmpxchg64;
    use crate::try_compare_exchange;
    use crate::try_memmove;
    use crate::try_memset;
    use crate::try_read8;
    use crate::try_read16;
    use crate::try_read32;
    use crate::try_read64;
    use crate::try_write8;
    use crate::try_write16;
    use crate::try_write32;
    use crate::try_write64;

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
        let dest_addr = std::ptr::from_mut(&mut dest);
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
        #[cfg_attr(windows, expect(clippy::needless_update))]
        LAST_ACCESS_FAILURE.set(AccessFailure {
            address: nonsense_addr.cast(),
            ..AccessFailure::empty()
        });

        let res = unsafe {
            match size {
                Size::Bit8 => match primitive {
                    Primitive::Read => try_read8(src_addr.cast()).map(|v| {
                        dest_addr.cast::<u8>().write(v);
                        true
                    }),
                    Primitive::Write => try_write8(dest_addr.cast(), src as u8).map(|()| true),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg8(dest_addr.cast(), &mut (dest as u8), src as u8)
                    }
                },
                Size::Bit16 => match primitive {
                    Primitive::Read => try_read16(src_addr.cast()).map(|v| {
                        dest_addr.cast::<u16>().write(v);
                        true
                    }),
                    Primitive::Write => try_write16(dest_addr.cast(), src as u16).map(|()| true),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg16(dest_addr.cast(), &mut (dest as u16), src as u16)
                    }
                },
                Size::Bit32 => match primitive {
                    Primitive::Read => try_read32(src_addr.cast()).map(|v| {
                        dest_addr.cast::<u32>().write(v);
                        true
                    }),
                    Primitive::Write => try_write32(dest_addr.cast(), src as u32).map(|()| true),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg32(dest_addr.cast(), &mut (dest as u32), src as u32)
                    }
                },
                Size::Bit64 => match primitive {
                    Primitive::Read => try_read64(src_addr.cast()).map(|v| {
                        dest_addr.write(v);
                        true
                    }),
                    Primitive::Write => try_write64(dest_addr.cast(), src).map(|()| true),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg64(dest_addr.cast(), &mut { dest }, src)
                    }
                },
            }
        };
        assert!(
            res.unwrap(),
            "Success should be returned for {primitive:?} and {size:?}"
        );
        assert_eq!(
            dest, expected,
            "Expected value must match the result for {primitive:?} and {size:?}"
        );
        assert_eq!(
            LAST_ACCESS_FAILURE.get().address,
            nonsense_addr.cast(),
            "Fault address must not be set for {primitive:?} and {size:?}"
        );

        let res = unsafe {
            match size {
                Size::Bit8 => match primitive {
                    Primitive::Read => try_read8(bad_addr.cast()).map(drop),
                    Primitive::Write => try_write8(bad_addr_mut.cast(), src as u8),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg8(bad_addr_mut.cast(), &mut (dest as u8), src as u8).map(drop)
                    }
                },
                Size::Bit16 => match primitive {
                    Primitive::Read => try_read16(bad_addr.cast()).map(drop),
                    Primitive::Write => try_write16(bad_addr_mut.cast(), src as u16),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg16(bad_addr_mut.cast(), &mut (dest as u16), src as u16).map(drop)
                    }
                },
                Size::Bit32 => match primitive {
                    Primitive::Read => try_read32(bad_addr.cast()).map(drop),
                    Primitive::Write => try_write32(bad_addr_mut.cast(), src as u32),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg32(bad_addr_mut.cast(), &mut (dest as u32), src as u32).map(drop)
                    }
                },
                Size::Bit64 => match primitive {
                    Primitive::Read => try_read64(bad_addr.cast()).map(drop),
                    Primitive::Write => try_write64(bad_addr_mut.cast(), src),
                    Primitive::CompareAndSwap => {
                        try_cmpxchg64(bad_addr_mut.cast(), &mut { dest }, src).map(drop)
                    }
                },
            }
        };
        res.unwrap_err();
        assert_eq!(
            dest, expected,
            "Fault preserved source and destination for {primitive:?} and {size:?}"
        );
        let af = LAST_ACCESS_FAILURE.get();
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

    #[test]
    fn test_try_memmove_nonoverlapping() {
        initialize_try_copy();
        let max = 8000;
        let src = (0..max).map(|x| (x % 256) as u8).collect::<Vec<u8>>();
        let mut dest = vec![0u8; max];
        for i in 0..max {
            let dest = &mut dest[max - i..];
            let src = &src[max - i..];
            dest.fill(0);
            unsafe {
                try_memmove(dest.as_mut_ptr(), src.as_ptr(), i).unwrap();
            };
            assert_eq!(dest, src);
        }
    }

    #[test]
    fn test_try_memmove_overlapping() {
        initialize_try_copy();

        let data = (0..256).map(|i| i as u8).collect::<Vec<_>>();

        // Reverse overlap
        {
            let mut buf = data.clone();
            unsafe { try_memmove(buf.as_mut_ptr(), buf.as_mut_ptr().add(1), 255).unwrap() };
            assert_eq!(&buf[0..255], &data[1..256]);
        }

        // Forward overlap
        {
            let mut buf = data.clone();
            unsafe { try_memmove(buf.as_mut_ptr().add(1), buf.as_mut_ptr(), 255).unwrap() };
            assert_eq!(&buf[1..256], &data[0..255]);
        }
    }

    #[test]
    fn test_try_memset() {
        initialize_try_copy();

        for c in [0, 0x5f] {
            for n in [
                0, 1, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256, 528, 1942, 4097,
            ] {
                let mut buf = vec![0u8; n];
                unsafe { try_memset(buf.as_mut_ptr(), c, n).unwrap() };
                assert_eq!(buf, vec![c; n]);
            }
        }
    }

    #[test]
    fn test_cmpxchg() {
        initialize_try_copy();

        let mut mapping = vec![0u64; 256];
        let base = mapping.as_mut_ptr().cast::<u8>();
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
            try_compare_exchange(0x1000 as *mut u8, 0, 2).unwrap_err();
        }
    }
}
