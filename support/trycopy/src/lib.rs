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
//!    execution to the recovery code. Any other fault is passed on: on Unix to
//!    the signal's previous action, such as the Rust runtime's stack overflow
//!    handler or the default action, after a line on standard error reports
//!    the fault if standard error takes it promptly, and on Windows to the
//!    next exception handler. It therefore terminates the process with the
//!    original signal or exception, as it would without these handlers. Like
//!    the kernel, the Unix handler applies the previous action's signal mask
//!    and runs a one-shot (`SA_RESETHAND`) action only for the first such
//!    fault.
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
            core::hint::cold_path();
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
            core::hint::cold_path();
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
            core::hint::cold_path();
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
            core::hint::cold_path();
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
            core::hint::cold_path();
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
        } else if let Some(src) = src
            && failure.is_write == Some(false)
            && (src..src.wrapping_add(len)).contains(&failure.address.cast_const())
        {
            // The address alone is ambiguous when the copy ranges overlap.
            (failure.address as usize - src as usize, false)
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
    /// None when the platform does not report an access direction.
    is_write: Option<bool>,
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
            is_write: None,
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

#[cfg(target_os = "linux")]
type Context = libc::mcontext_t;
#[cfg(target_os = "macos")]
type Context = libc::__darwin_mcontext64;
#[cfg(windows)]
type Context = windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

/// The signals that report the access faults that trycopy recovers from.
#[cfg(unix)]
const HANDLED_SIGNALS: &[libc::c_int] = &[
    libc::SIGSEGV,
    libc::SIGBUS,
    // SIGILL is used on Linux/aarch64 to signal certain memory access
    // faults from a hypervisor.
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    libc::SIGILL,
];

/// The actions that [`HANDLED_SIGNALS`] had before trycopy installed its
/// handler, in the same order.
#[cfg(unix)]
static PREVIOUS_ACTIONS: std::sync::OnceLock<Vec<libc::sigaction>> = std::sync::OnceLock::new();

/// The executable's load bias and the address range of its loadable segments,
/// used to report a faulting instruction as an offset in the executable.
#[cfg(target_os = "linux")]
static EXECUTABLE_LAYOUT: std::sync::OnceLock<(usize, std::ops::Range<usize>)> =
    std::sync::OnceLock::new();

/// Returns the executable's load bias and the address range of its loadable
/// segments, as the C runtime describes the executable: the first object that
/// `dl_iterate_phdr` reports.
///
/// The C runtime determines the bias of every kind of executable. The program
/// headers in the auxiliary vector give it only through their `PT_PHDR`
/// entry, which the GNU linker omits from an executable without an
/// interpreter. A static PIE executable, such as Rust links for
/// `x86_64-unknown-linux-musl`, lacks it, although it is loaded at a nonzero
/// address.
///
/// Not async-signal-safe, since `dl_iterate_phdr` may take a lock.
#[cfg(target_os = "linux")]
fn executable_layout() -> Option<(usize, std::ops::Range<usize>)> {
    /// Stores the layout of the first object in the `Option` that `data`
    /// points to, and stops the iteration.
    ///
    /// # Safety
    /// Must only be called by `dl_iterate_phdr`, with `data` pointing to an
    /// `Option<(usize, Range<usize>)>`.
    unsafe extern "C" fn first_object(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: `dl_iterate_phdr` passes a valid description of a loaded
        // object, and the caller passes `data` as documented.
        let (info, layout) = unsafe {
            (
                &*info,
                &mut *data.cast::<Option<(usize, std::ops::Range<usize>)>>(),
            )
        };
        if !info.dlpi_phdr.is_null() {
            // SAFETY: the object's program headers stay mapped while it is
            // loaded.
            let headers =
                unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum.into()) };
            let bias = info.dlpi_addr as usize;
            let (start, end) = headers.iter().filter(|h| h.p_type == libc::PT_LOAD).fold(
                (usize::MAX, 0),
                |(start, end), h| {
                    (
                        start.min(h.p_vaddr as usize),
                        end.max(h.p_vaddr.wrapping_add(h.p_memsz) as usize),
                    )
                },
            );
            *layout =
                (start < end).then(|| (bias, bias.wrapping_add(start)..bias.wrapping_add(end)));
        }
        // Stop after the first object, which is the executable.
        1
    }

    let mut layout = None;
    // SAFETY: `first_object` is passed a pointer to the layout, as it requires.
    unsafe { libc::dl_iterate_phdr(Some(first_object), (&raw mut layout).cast()) };
    layout
}

/// A line of text built without allocating, for use in a signal handler.
#[cfg(unix)]
struct SignalSafeLine {
    buf: [u8; SIGNAL_SAFE_LINE_CAPACITY],
    len: usize,
}

/// The longest [`SignalSafeLine`]. It is shorter than `PIPE_BUF`, so that a
/// single `write` adds a line to a pipe whole.
#[cfg(unix)]
const SIGNAL_SAFE_LINE_CAPACITY: usize = 192;
#[cfg(unix)]
const _: () = assert!(SIGNAL_SAFE_LINE_CAPACITY <= libc::PIPE_BUF);

#[cfg(unix)]
impl SignalSafeLine {
    fn new() -> Self {
        Self {
            buf: [0; SIGNAL_SAFE_LINE_CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> &mut Self {
        let len = bytes.len().min(self.buf.len() - self.len);
        self.buf[self.len..][..len].copy_from_slice(&bytes[..len]);
        self.len += len;
        self
    }

    fn push_hex(&mut self, value: usize) -> &mut Self {
        let mut digits = [b'0'; 2 + 2 * size_of::<usize>()];
        digits[1] = b'x';
        for (i, digit) in digits[2..].iter_mut().rev().enumerate() {
            *digit = b"0123456789abcdef"[(value >> (4 * i)) & 0xf];
        }
        self.push(&digits)
    }

    fn push_decimal(&mut self, value: i32) -> &mut Self {
        if value < 0 {
            self.push(b"-");
        }
        let mut value = value.unsigned_abs();
        let mut digits = [0; 10];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push(&digits[start..])
    }
}

/// How long, in milliseconds, a report waits for standard error to take it.
#[cfg(unix)]
const REPORT_TIMEOUT_MS: libc::c_int = 100;

/// Reports a fault that no recovery region covers on standard error, with
/// the signal, its code, the faulting address (if the signal has one), and
/// the faulting instruction. On Linux, the report also gives the instruction
/// as an offset in the executable, which a symbolizer such as `addr2line` can
/// resolve.
///
/// The report is best-effort: it is skipped if standard error cannot take it
/// within [`REPORT_TIMEOUT_MS`], so that a stalled reader of a full pipe does
/// not keep the fault from being passed on.
///
/// Uses only async-signal-safe operations.
#[cfg(unix)]
fn report_unrecovered_fault(sig: libc::c_int, code: libc::c_int, address: *mut u8, ip: usize) {
    let name: &[u8] = match sig {
        libc::SIGSEGV => b"SIGSEGV",
        libc::SIGBUS => b"SIGBUS",
        libc::SIGILL => b"SIGILL",
        _ => b"signal",
    };
    let mut line = SignalSafeLine::new();
    line.push(b"trycopy: unrecovered ")
        .push(name)
        .push(b" (code ")
        .push_decimal(code)
        .push(b")");
    if sig == libc::SIGSEGV || sig == libc::SIGBUS {
        line.push(b" at address ").push_hex(address.addr());
    }
    line.push(b" from instruction ").push_hex(ip);
    #[cfg(target_os = "linux")]
    if let Some((bias, range)) = EXECUTABLE_LAYOUT.get() {
        if range.contains(&ip) {
            line.push(b" (executable offset ")
                .push_hex(ip.wrapping_sub(*bias))
                .push(b")");
        }
    }
    line.push(b"\n");
    // Write the line with a single `write`, without retrying, and only if
    // `poll` reports that standard error can take it, after waiting briefly
    // for room. Setting `O_NONBLOCK` instead would change standard error's
    // open file description, which other processes may share. A pipe that
    // `poll` reports writable takes a line shorter than `PIPE_BUF` whole
    // without blocking, unless another thread fills it between the `poll` and
    // the `write`. An error or a hang-up skips the line too, so that a pipe
    // without a reader does not raise SIGPIPE.
    let mut stderr = libc::pollfd {
        fd: libc::STDERR_FILENO,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `poll` is async-signal-safe, and `stderr` is a valid `pollfd`.
    let writable = unsafe { libc::poll(&mut stderr, 1, REPORT_TIMEOUT_MS) } == 1
        && stderr.revents & libc::POLLOUT != 0
        && stderr.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0;
    if writable {
        // SAFETY: `write` is async-signal-safe, and the buffer is valid for
        // `line.len` bytes. Nothing can be done if the write fails.
        unsafe { libc::write(libc::STDERR_FILENO, line.buf.as_ptr().cast(), line.len) };
    }
}

/// Passes a fault that no recovery region covers to the action that `sig` had
/// before trycopy installed its handler, so that trycopy does not change how
/// the fault terminates the process.
///
/// That action is typically the Rust runtime's handler, which reports a stack
/// overflow and aborts, and otherwise restores the default action, as this
/// function does when there was no handler. The default action terminates the
/// process with `sig` and dumps the faulting instruction's state when that
/// instruction faults again after the handler returns. As with the Rust
/// runtime's handler alone, if another thread has made the address accessible
/// in the meantime, execution continues with the default action installed.
///
/// The kernel resets a one-shot (`SA_RESETHAND`) action to the default action
/// before it runs the action's handler, so that the handler runs only once and
/// a fault that it returns without repairing then terminates the process.
/// Resetting trycopy's handler would stop it from recovering later faults, so
/// instead, only the first fault to get here, on any thread, runs such a
/// handler, and any later one gets the default action.
///
/// The handler runs with the signal mask that the kernel would have given it,
/// as [`handler_mask`] describes, and trycopy's mask is restored when it
/// returns, so a fault while it runs is handled as it would be without
/// trycopy: with `SA_NODEFER`, it reaches trycopy's handler, and then the
/// action's handler, again.
///
/// The handler also starts with `saved_errno`, the interrupted code's `errno`,
/// as it would if the kernel ran it, although trycopy's calls may have changed
/// `errno` since. It may leave with `siglongjmp`, as from a handler that the
/// kernel ran, since trycopy's handler holds nothing with a destructor, and if
/// it first restores the `errno` that it started with, the code where it
/// resumes sees the interrupted value.
///
/// The action's other flags take no effect: the handler runs on the thread's
/// alternate signal stack, if it has one, even without `SA_ONSTACK`, since
/// trycopy's action has it so that a stack overflow is still reported, and
/// `SA_RESTART` would only restart a system call that a signal sent with
/// `kill` interrupted, as a fault interrupts none.
///
/// # Safety
/// Must be called from the handler for `sig`, with that handler's arguments.
#[cfg(unix)]
unsafe fn chain_fault(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
    saved_errno: libc::c_int,
) {
    type SigInfoHandler =
        unsafe extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);
    type Handler = unsafe extern "C" fn(libc::c_int);

    /// Whether the one-shot handler of each of [`HANDLED_SIGNALS`], in the
    /// same order, has run. A fault claims a handler with an atomic swap, so
    /// that only one fault runs it, even when several threads fault at once.
    /// The swap is lock-free, and thus async-signal-safe.
    static ONE_SHOT_RAN: [std::sync::atomic::AtomicBool; HANDLED_SIGNALS.len()] =
        [const { std::sync::atomic::AtomicBool::new(false) }; HANDLED_SIGNALS.len()];

    let previous = HANDLED_SIGNALS
        .iter()
        .position(|&handled| handled == sig)
        .and_then(|index| {
            Some((
                PREVIOUS_ACTIONS.get()?.get(index)?,
                ONE_SHOT_RAN.get(index)?,
            ))
        });
    match previous {
        Some((action, one_shot_ran))
            if action.sa_sigaction != libc::SIG_DFL
                && action.sa_sigaction != libc::SIG_IGN
                && (action.sa_flags & libc::SA_RESETHAND == 0
                    || !one_shot_ran.swap(true, std::sync::atomic::Ordering::Relaxed)) =>
        {
            // Run the handler with the signal mask that the kernel would have
            // given it, and then restore trycopy's.
            //
            // SAFETY: `ucontext` is the user context of the handler for `sig`,
            // and `pthread_sigmask` is async-signal-safe.
            let trycopy_mask = unsafe {
                let interrupted = &(*ucontext.cast::<libc::ucontext_t>()).uc_sigmask;
                let mut trycopy_mask = std::mem::zeroed();
                libc::pthread_sigmask(
                    libc::SIG_SETMASK,
                    &handler_mask(sig, action, interrupted),
                    &mut trycopy_mask,
                );
                trycopy_mask
            };
            // Give the handler the interrupted code's `errno` last, after every
            // call that may change it.
            //
            // SAFETY: `errno_location` points to the calling thread's `errno`.
            unsafe { errno_location().write(saved_errno) };
            // SAFETY: `sa_sigaction` holds a handler of the type that
            // `SA_SIGINFO` selects, which is called with the arguments of the
            // signal being handled, as the kernel would have called it.
            unsafe {
                if action.sa_flags & libc::SA_SIGINFO != 0 {
                    std::mem::transmute::<libc::sighandler_t, SigInfoHandler>(action.sa_sigaction)(
                        sig, info, ucontext,
                    );
                } else {
                    std::mem::transmute::<libc::sighandler_t, Handler>(action.sa_sigaction)(sig);
                }
            }
            // SAFETY: `pthread_sigmask` is async-signal-safe, and
            // `trycopy_mask` holds the mask that it returned.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &trycopy_mask, std::ptr::null_mut())
            };
        }
        _ => {
            // Restore the default action. A synchronous fault cannot be
            // ignored, so returning re-executes the faulting instruction, which
            // then terminates the process with `sig`.
            //
            // SAFETY: `sigaction` is async-signal-safe, and a zeroed action
            // with `SIG_DFL` is valid.
            unsafe {
                let action = libc::sigaction {
                    sa_sigaction: libc::SIG_DFL,
                    ..core::mem::zeroed()
                };
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    }
}

/// Returns the signal mask that the kernel sets for the handler of `action`
/// when it delivers `sig` to a thread whose mask is `interrupted`:
/// `interrupted`, the action's `sa_mask`, and `sig`, unless the action has
/// `SA_NODEFER`.
///
/// Uses only async-signal-safe functions. There is no portable union of
/// signal sets, so this tests each signal number.
#[cfg(unix)]
fn handler_mask(
    sig: libc::c_int,
    action: &libc::sigaction,
    interrupted: &libc::sigset_t,
) -> libc::sigset_t {
    /// The signal numbers: up to 64 on Linux, which has real-time signals, and
    /// up to 31 on macOS.
    #[cfg(target_os = "linux")]
    const SIGNALS: std::ops::Range<libc::c_int> = 1..65;
    #[cfg(target_os = "macos")]
    const SIGNALS: std::ops::Range<libc::c_int> = 1..32;

    // SAFETY: the signal sets are valid, and these functions are
    // async-signal-safe.
    unsafe {
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        for signal in SIGNALS {
            if libc::sigismember(interrupted, signal) == 1
                || libc::sigismember(&action.sa_mask, signal) == 1
            {
                libc::sigaddset(&mut mask, signal);
            }
        }
        if action.sa_flags & libc::SA_NODEFER == 0 {
            libc::sigaddset(&mut mask, sig);
        }
        mask
    }
}

/// Returns the location of the calling thread's `errno`.
#[cfg(unix)]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: these functions have no preconditions.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::__errno_location()
        }
        #[cfg(target_os = "macos")]
        {
            libc::__error()
        }
    }
}

/// # Safety
/// This function installs global signal handlers. It must only be called once.
#[cfg(unix)]
unsafe fn install_signal_handlers() {
    /// # Safety
    /// Must only be called by the kernel as an `SA_SIGINFO` signal handler.
    unsafe extern "C" fn handle_signal(
        sig: libc::c_int,
        info_ptr: *mut libc::siginfo_t,
        ucontext_ptr: *mut libc::c_void,
    ) {
        // The interrupted code continues after a recovery, or after a previous
        // handler repairs a fault, so preserve its `errno`, which the calls
        // below can change, and give it to the previous handler too. It is
        // restored explicitly, not by a destructor, so that a previous handler
        // can still leave with `siglongjmp`.
        let errno = errno_location();
        // SAFETY: `errno` points to the calling thread's `errno`.
        let saved_errno = unsafe { errno.read() };

        // SAFETY: an `SA_SIGINFO` handler receives valid signal information
        // and an exclusively borrowed user context.
        let (info, ucontext) =
            unsafe { (&*info_ptr, &mut *ucontext_ptr.cast::<libc::ucontext_t>()) };

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

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        // The W/R bit is valid for page faults, not other x86 exceptions.
        let is_write = (ctx.gregs[libc::REG_TRAPNO as usize] == 14)
            .then_some(ctx.gregs[libc::REG_ERR as usize] & 2 != 0);

        #[cfg(target_arch = "aarch64")]
        let esr = {
            #[cfg(target_os = "linux")]
            {
                const ESR_MAGIC: u32 = 0x45535201;
                aarch64_extended_context::<u64>(ctx, ESR_MAGIC).copied()
            }
            #[cfg(target_os = "macos")]
            {
                Some(u64::from(ctx.__es.__esr))
            }
        };
        #[cfg(target_arch = "aarch64")]
        // WnR is defined for data aborts from a lower or the current EL.
        let is_write = esr.and_then(|esr| {
            matches!((esr >> 26) & 0x3f, 0x24 | 0x25).then_some(esr & (1 << 6) != 0)
        });

        let failure = AccessFailure {
            address,
            is_write,
            si_signo: sig,
            si_code: info.si_code as u32,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            trapno: ctx.gregs[libc::REG_TRAPNO as usize] as u8,
            #[cfg(target_arch = "aarch64")]
            esr,
        };

        let recovered = recover(ctx, failure);
        if !recovered {
            report_unrecovered_fault(sig, info.si_code, address, get_context_ip(ctx));
            // SAFETY: called from the handler for `sig` with its arguments.
            unsafe { chain_fault(sig, info_ptr, ucontext_ptr, saved_errno) };
        }

        // SAFETY: `errno` points to the calling thread's `errno`.
        unsafe { errno.write(saved_errno) };
    }

    // Determine the executable's layout here, not in the handler, which only
    // reads it: `executable_layout` is not async-signal-safe.
    #[cfg(target_os = "linux")]
    if let Some(layout) = executable_layout() {
        let _ = EXECUTABLE_LAYOUT.set(layout);
    }

    // SAFETY: installing signal handlers as documented. The previous actions
    // are recorded before the handlers that read them are installed.
    unsafe {
        let previous = HANDLED_SIGNALS
            .iter()
            .map(|&signal| {
                let mut previous: libc::sigaction = core::mem::zeroed();
                libc::sigaction(signal, std::ptr::null(), &mut previous);
                previous
            })
            .collect();
        assert!(
            PREVIOUS_ACTIONS.set(previous).is_ok(),
            "signal handlers are installed only once"
        );

        let act = libc::sigaction {
            sa_sigaction: handle_signal as *const () as usize,
            // Run on the thread's alternate signal stack, if it has one, so
            // that a stack overflow still reaches the Rust runtime's handler.
            sa_flags: libc::SA_SIGINFO | libc::SA_ONSTACK,
            ..core::mem::zeroed()
        };
        for &signal in HANDLED_SIGNALS {
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
            is_write: match record.ExceptionInformation[0] {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
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
    use crate::try_copy;
    use crate::try_memmove;
    use crate::try_memset;
    use crate::try_read_volatile;
    use crate::try_read8;
    use crate::try_read16;
    use crate::try_read32;
    use crate::try_read64;
    use crate::try_write_bytes;
    use crate::try_write_volatile;
    use crate::try_write8;
    use crate::try_write16;
    use crate::try_write32;
    use crate::try_write64;
    use test_with_tracing::test;

    struct GuardedMapping {
        ptr: *mut u8,
        page_size: usize,
    }

    impl GuardedMapping {
        #[cfg(unix)]
        fn new() -> Self {
            initialize_try_copy();
            // SAFETY: reserve three owned pages, then protect the middle page.
            unsafe {
                let page_size = usize::try_from(libc::sysconf(libc::_SC_PAGESIZE)).unwrap();
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    3 * page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                assert_ne!(ptr, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
                let mapping = Self {
                    ptr: ptr.cast(),
                    page_size,
                };
                assert_eq!(
                    libc::mprotect(ptr.add(page_size), page_size, libc::PROT_NONE),
                    0,
                    "{}",
                    std::io::Error::last_os_error()
                );
                mapping
            }
        }

        #[cfg(windows)]
        fn new() -> Self {
            use windows_sys::Win32::System::Memory;
            use windows_sys::Win32::System::SystemInformation::GetSystemInfo;

            initialize_try_copy();
            // SAFETY: reserve three owned pages, then protect the middle page.
            unsafe {
                let mut info = std::mem::zeroed();
                GetSystemInfo(&mut info);
                let page_size = info.dwPageSize as usize;
                let ptr = Memory::VirtualAlloc(
                    std::ptr::null(),
                    3 * page_size,
                    Memory::MEM_RESERVE | Memory::MEM_COMMIT,
                    Memory::PAGE_READWRITE,
                );
                assert!(!ptr.is_null(), "{}", std::io::Error::last_os_error());
                let mapping = Self {
                    ptr: ptr.cast(),
                    page_size,
                };
                let mut old_protection = 0;
                assert_ne!(
                    Memory::VirtualProtect(
                        ptr.add(page_size),
                        page_size,
                        Memory::PAGE_NOACCESS,
                        &mut old_protection,
                    ),
                    0,
                    "{}",
                    std::io::Error::last_os_error()
                );
                mapping
            }
        }
    }

    impl Drop for GuardedMapping {
        fn drop(&mut self) {
            #[cfg(unix)]
            // SAFETY: this object owns the complete three-page reservation.
            assert_eq!(
                unsafe { libc::munmap(self.ptr.cast(), 3 * self.page_size) },
                0
            );
            #[cfg(windows)]
            // SAFETY: this object owns the reservation, released in its entirety.
            assert_ne!(
                unsafe {
                    windows_sys::Win32::System::Memory::VirtualFree(
                        self.ptr.cast(),
                        0,
                        windows_sys::Win32::System::Memory::MEM_RELEASE,
                    )
                },
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Selects the fault that [`unrecovered_fault_child`] raises in a child
    /// process.
    #[cfg(target_os = "linux")]
    const UNRECOVERED_FAULT_CHILD: &str = "TRYCOPY_TEST_UNRECOVERED_FAULT";

    /// Returns a command that runs [`unrecovered_fault_child`] in a child
    /// process that raises `fault`.
    #[cfg(target_os = "linux")]
    fn unrecovered_fault_child_command(fault: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::unrecovered_fault_child", "--nocapture"])
            .env(UNRECOVERED_FAULT_CHILD, fault);
        command
    }

    /// Runs [`unrecovered_fault_child`] in a child process that raises
    /// `fault`, returning the child's exit status and standard error.
    #[cfg(target_os = "linux")]
    fn run_unrecovered_fault_child(fault: &str) -> (std::process::ExitStatus, String) {
        let output = unrecovered_fault_child_command(fault).output().unwrap();
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Recurses until the stack overflows.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[inline(never)]
    fn overflow_stack(depth: u64) -> u64 {
        let frame = std::hint::black_box([depth; 64]);
        if frame[0] == u64::MAX {
            return 0;
        }
        overflow_stack(depth + 1).wrapping_add(frame[63])
    }

    /// The line that [`one_shot_handler`] writes to standard error when it
    /// runs.
    #[cfg(target_os = "linux")]
    const ONE_SHOT_HANDLER_MARKER: &str = "one-shot handler ran\n";

    /// A one-shot (`SA_RESETHAND`) SIGSEGV handler that writes
    /// [`ONE_SHOT_HANDLER_MARKER`] and returns without repairing the fault. It
    /// exits with status 99 if it runs again, which the kernel would not let
    /// it do, so that the fault does not repeat forever.
    #[cfg(target_os = "linux")]
    extern "C" fn one_shot_handler(_: libc::c_int) {
        static RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if RAN.swap(true, std::sync::atomic::Ordering::Relaxed) {
            // SAFETY: `_exit` is async-signal-safe.
            unsafe { libc::_exit(99) };
        }
        // SAFETY: `write` is async-signal-safe, and the marker is valid for its
        // length.
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                ONE_SHOT_HANDLER_MARKER.as_ptr().cast(),
                ONE_SHOT_HANDLER_MARKER.len(),
            )
        };
    }

    /// Installs [`one_shot_handler`] as the SIGSEGV handler, with
    /// `SA_RESETHAND`.
    #[cfg(target_os = "linux")]
    fn install_one_shot_handler() {
        // SAFETY: the handler calls only async-signal-safe functions, and a
        // zeroed action is valid.
        let result = unsafe {
            libc::sigaction(
                libc::SIGSEGV,
                &libc::sigaction {
                    sa_sigaction: one_shot_handler as *const () as libc::sighandler_t,
                    sa_flags: libc::SA_RESETHAND,
                    ..std::mem::zeroed()
                },
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }

    /// The exit status of [`mask_checking_handler`].
    #[cfg(target_os = "linux")]
    const MASK_CHECKING_HANDLER_EXIT: i32 = 42;

    /// A SIGSEGV handler that writes which of SIGUSR1, SIGUSR2, SIGSEGV, and
    /// SIGTERM it runs with blocked, and exits with
    /// [`MASK_CHECKING_HANDLER_EXIT`].
    #[cfg(target_os = "linux")]
    extern "C" fn mask_checking_handler(
        _: libc::c_int,
        _: *mut libc::siginfo_t,
        _: *mut libc::c_void,
    ) {
        let put = |bytes: &[u8]| {
            // SAFETY: `write` is async-signal-safe, and `bytes` is valid.
            unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        };
        // SAFETY: a zeroed signal set is valid, and `pthread_sigmask`,
        // `sigismember`, and `_exit` are async-signal-safe.
        unsafe {
            let mut blocked = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut blocked);
            put(b"previous handler blocks:");
            for (signal, name) in [
                (libc::SIGUSR1, b" SIGUSR1"),
                (libc::SIGUSR2, b" SIGUSR2"),
                (libc::SIGSEGV, b" SIGSEGV"),
                (libc::SIGTERM, b" SIGTERM"),
            ] {
                if libc::sigismember(&blocked, signal) == 1 {
                    put(name);
                }
            }
            put(b"\n");
            libc::_exit(MASK_CHECKING_HANDLER_EXIT);
        }
    }

    /// Installs [`mask_checking_handler`] as the SIGSEGV handler, with
    /// `SA_SIGINFO`, `flags`, and SIGUSR1 in its mask, and blocks SIGUSR2 in
    /// the calling thread, which is to fault.
    #[cfg(target_os = "linux")]
    fn install_mask_checking_handler(flags: libc::c_int) {
        // SAFETY: the handler calls only async-signal-safe functions, and each
        // signal set is initialized before it is used.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = mask_checking_handler as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_SIGINFO | flags;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
            assert_eq!(
                libc::sigaction(libc::SIGSEGV, &action, std::ptr::null_mut()),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
            let mut blocked = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGUSR2);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()),
                0
            );
        }
    }

    /// The page size, for [`repairing_handler`].
    #[cfg(target_os = "linux")]
    static REPAIR_PAGE_SIZE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// The `errno` that [`repairing_handler`] started with, or -1 before it
    /// runs.
    #[cfg(target_os = "linux")]
    static REPAIR_ENTRY_ERRNO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    /// A SIGSEGV handler that records the `errno` that it starts with in
    /// [`REPAIR_ENTRY_ERRNO`] and repairs a fault by making the faulting page
    /// readable, and exits with status 98 if it cannot.
    #[cfg(target_os = "linux")]
    extern "C" fn repairing_handler(
        _: libc::c_int,
        info: *mut libc::siginfo_t,
        _: *mut libc::c_void,
    ) {
        // SAFETY: `__errno_location` has no preconditions, and it points to
        // this thread's `errno`.
        let entry_errno = unsafe { libc::__errno_location().read() };
        REPAIR_ENTRY_ERRNO.store(entry_errno, std::sync::atomic::Ordering::Relaxed);
        let page_size = REPAIR_PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed);
        // SAFETY: an `SA_SIGINFO` handler receives valid signal information,
        // and `mprotect` and `_exit` only make system calls.
        unsafe {
            let page = (*info).si_addr().map_addr(|addr| addr & !(page_size - 1));
            if libc::mprotect(page, page_size, libc::PROT_READ) != 0 {
                libc::_exit(98);
            }
        }
    }

    /// Installs [`repairing_handler`] as the SIGSEGV handler.
    #[cfg(target_os = "linux")]
    fn install_repairing_handler() {
        // SAFETY: `sysconf` has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        REPAIR_PAGE_SIZE.store(
            usize::try_from(page_size).unwrap(),
            std::sync::atomic::Ordering::Relaxed,
        );
        // SAFETY: the handler calls only functions that make system calls,
        // and a zeroed action is valid.
        let result = unsafe {
            libc::sigaction(
                libc::SIGSEGV,
                &libc::sigaction {
                    sa_sigaction: repairing_handler as *const () as libc::sighandler_t,
                    sa_flags: libc::SA_SIGINFO,
                    ..std::mem::zeroed()
                },
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }

    /// In a child process started by [`run_unrecovered_fault_child`], raises
    /// a fault outside any recovery region after installing the trycopy
    /// handlers, and for `one_shot_handler`, [`one_shot_handler`] before them.
    /// Does nothing in the test process itself.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_child() {
        let Some(fault) = std::env::var_os(UNRECOVERED_FAULT_CHILD) else {
            return;
        };
        // Don't leave a core dump behind for an expected crash.
        //
        // SAFETY: PR_SET_DUMPABLE takes no pointers and affects only this
        // child process.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) }, 0);
        match fault.to_str().unwrap() {
            "one_shot_handler" => install_one_shot_handler(),
            "masked_handler" => install_mask_checking_handler(0),
            "nodefer_handler" => install_mask_checking_handler(libc::SA_NODEFER),
            "repaired_access" => install_repairing_handler(),
            _ => {}
        }
        initialize_try_copy();
        match fault.to_str().unwrap() {
            "access" | "one_shot_handler" | "masked_handler" | "nodefer_handler" => {
                let mapping = GuardedMapping::new();
                // SAFETY: the middle page is within the reservation.
                let address = unsafe { mapping.ptr.add(mapping.page_size) };
                eprintln!("faulting at address {:#018x}", address.addr());
                // SAFETY: the middle page is reserved but inaccessible, and no
                // recovery region covers this read.
                let _value = unsafe { std::ptr::read_volatile(address) };
            }
            "silent_access" => {
                // Write nothing first: standard error may be full.
                let mapping = GuardedMapping::new();
                // SAFETY: the middle page is reserved but inaccessible, and no
                // recovery region covers this read.
                let _value = unsafe { std::ptr::read_volatile(mapping.ptr.add(mapping.page_size)) };
            }
            "repaired_access" => {
                let mapping = GuardedMapping::new();
                // SAFETY: `__errno_location` has no preconditions.
                let errno = unsafe { libc::__errno_location() };
                // SAFETY: `errno` points to this thread's `errno`, and the
                // middle page is within the reservation. No recovery region
                // covers the read, but the previous handler makes the page
                // readable when it faults.
                let errno_after = unsafe {
                    errno.write_volatile(libc::EDOM);
                    std::ptr::read_volatile(mapping.ptr.add(mapping.page_size));
                    errno.read_volatile()
                };
                println!(
                    "errno at the previous handler's entry: {}",
                    REPAIR_ENTRY_ERRNO.load(std::sync::atomic::Ordering::Relaxed)
                );
                println!("errno after the repaired fault: {errno_after}");
                // SAFETY: `_exit` has no preconditions.
                unsafe { libc::_exit(0) };
            }
            #[cfg(target_env = "gnu")]
            "stack_overflow" => {
                std::thread::Builder::new()
                    .stack_size(256 * 1024)
                    .spawn(|| overflow_stack(0))
                    .unwrap()
                    .join()
                    .unwrap();
            }
            fault => panic!("unknown fault {fault}"),
        }
        panic!("the fault did not terminate the process");
    }

    /// A fault outside a recovery region terminates the process with its own
    /// signal, as it would without trycopy, rather than with SIGABRT.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_terminates_with_its_signal() {
        use std::os::unix::process::ExitStatusExt;

        let (status, stderr) = run_unrecovered_fault_child("access");
        assert_eq!(status.signal(), Some(libc::SIGSEGV), "{status}\n{stderr}");
    }

    /// A fault outside a recovery region is reported on standard error with its
    /// code, its address, and the faulting instruction's executable offset.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_is_reported() {
        let (_, stderr) = run_unrecovered_fault_child("access");
        let address = stderr
            .lines()
            .find_map(|line| line.strip_prefix("faulting at address "))
            .unwrap_or_else(|| panic!("{stderr}"));
        let report = stderr
            .lines()
            .find(|line| line.starts_with("trycopy: "))
            .unwrap_or_else(|| panic!("{stderr}"));
        // An access to an inaccessible page reports SEGV_ACCERR.
        let prefix = format!("trycopy: unrecovered SIGSEGV (code 2) at address {address} ");
        assert!(report.starts_with(&prefix), "{report}");
        assert!(report.contains(" (executable offset 0x"), "{report}");
    }

    /// A fault outside a recovery region terminates the process although
    /// standard error is a full pipe whose reader has stalled: the report waits
    /// only briefly for room, and is then skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_terminates_with_full_stderr() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        use std::time::Duration;
        use std::time::Instant;

        let (mut reader, writer) = std::io::pipe().unwrap();
        // Fill the pipe without blocking, and then let writes to it block
        // again, as they do to a pipe whose reader has stalled.
        let fd = writer.as_raw_fd();
        // SAFETY: `fd` is the pipe's open write end, and `fill` is valid.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_ne!(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK), -1);
            let fill = [b'x'; 4096];
            for len in [fill.len(), 1] {
                while libc::write(fd, fill.as_ptr().cast(), len) > 0 {}
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EAGAIN)
                );
            }
            assert_ne!(libc::fcntl(fd, libc::F_SETFL, flags), -1);
        }
        let mut child = unrecovered_fault_child_command("silent_access")
            .stdout(std::process::Stdio::null())
            .stderr(writer)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("the child did not terminate within 10 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(libc::SIGSEGV), "{status}");
        // The child wrote nothing to the pipe: the report was skipped.
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert!(
            output.iter().all(|&byte| byte == b'x'),
            "{}",
            String::from_utf8_lossy(&output)
        );
    }

    /// A fault outside a recovery region that a previous handler repairs leaves
    /// `errno` as it was, both when that handler starts and after it returns,
    /// although writing the report fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn repaired_fault_preserves_errno() {
        // Writes to `/dev/full` fail with ENOSPC.
        let output = unrecovered_fault_child_command("repaired_access")
            .stderr(
                std::fs::File::options()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(0), "{}\n{stdout}", output.status);
        for when in [
            "at the previous handler's entry",
            "after the repaired fault",
        ] {
            let line = format!("errno {when}: {}\n", libc::EDOM);
            assert!(stdout.contains(&line), "{stdout}");
        }
    }

    /// A stack overflow still reaches the Rust runtime's handler, which
    /// reports it, once trycopy handles SIGSEGV.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn unrecovered_stack_overflow_is_reported() {
        use std::os::unix::process::ExitStatusExt;

        let (status, stderr) = run_unrecovered_fault_child("stack_overflow");
        assert_eq!(status.signal(), Some(libc::SIGABRT), "{status}\n{stderr}");
        assert!(stderr.contains("has overflowed its stack"), "{stderr}");
    }

    /// A one-shot (`SA_RESETHAND`) previous handler runs only for the first
    /// fault outside a recovery region, as it would without trycopy. When it
    /// returns without repairing the fault, the fault terminates the process
    /// with its own signal instead of running the handler again.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_runs_one_shot_handler_once() {
        use std::os::unix::process::ExitStatusExt;

        let (status, stderr) = run_unrecovered_fault_child("one_shot_handler");
        assert_eq!(status.signal(), Some(libc::SIGSEGV), "{status}\n{stderr}");
        assert_eq!(
            stderr.matches(ONE_SHOT_HANDLER_MARKER).count(),
            1,
            "{stderr}"
        );
    }

    /// A previous handler runs with the signal mask that the kernel would have
    /// given it: the interrupted thread's mask, its action's mask, and the
    /// signal itself, unless the action has `SA_NODEFER`.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrecovered_fault_runs_previous_handler_with_its_mask() {
        for (fault, blocked) in [
            ("masked_handler", "SIGUSR1 SIGUSR2 SIGSEGV"),
            ("nodefer_handler", "SIGUSR1 SIGUSR2"),
        ] {
            let (status, stderr) = run_unrecovered_fault_child(fault);
            assert_eq!(
                status.code(),
                Some(MASK_CHECKING_HANDLER_EXIT),
                "{fault}: {status}\n{stderr}"
            );
            let line = format!("previous handler blocks: {blocked}\n");
            assert!(stderr.contains(&line), "{fault}: {stderr}");
        }
    }

    #[test]
    fn test_overlapping_copy_reports_source_fault() {
        let mapping = GuardedMapping::new();
        let mut dest = [0; 3];
        // SAFETY: both copy ranges are reserved, overlap is permitted, and no
        // Rust references alias the mapping.
        unsafe {
            let src = mapping.ptr.add(mapping.page_size - 1);
            let control = try_copy(src, dest.as_mut_ptr(), 3).unwrap_err();
            assert_eq!(control.offset(), 1);
            assert!(!control.is_write);

            let error = try_copy(src, src.sub(1), 3).unwrap_err();
            assert_eq!(error.offset(), 1, "{error:?}");
            assert!(!error.is_write);
            assert_eq!(error.to_string(), "failed to read memory");
        }
    }

    #[test]
    fn test_public_copy_and_fill_element_counts() {
        initialize_try_copy();
        let src = std::array::from_fn::<_, 19, _>(|i| i as u16);
        let mut dest = [0xfeedu16; 21];
        let mut expected = dest;
        // SAFETY: the source and destination ranges are within the owned arrays.
        unsafe {
            try_copy(src.as_ptr(), dest.as_mut_ptr().add(1), src.len()).unwrap();
            expected[1..20].copy_from_slice(&src);
            assert_eq!(dest, expected);

            try_write_bytes(dest.as_mut_ptr().add(5), 0xa5, 3).unwrap();
            expected[5..8].fill(0xa5a5);
            assert_eq!(dest, expected);

            try_copy(src.as_ptr(), dest.as_mut_ptr(), 0).unwrap();
            try_write_bytes(dest.as_mut_ptr(), 0, 0).unwrap();
            assert_eq!(dest, expected);
        }
    }

    #[test]
    fn test_public_volatile_access_sizes() {
        fn check<T: Copy + Default + PartialEq + std::fmt::Debug>(value: T) {
            let mut dest = T::default();
            // SAFETY: the typed source and destination are valid and exclusively owned.
            unsafe {
                try_write_volatile(&mut dest, &value).unwrap();
                assert_eq!(dest, value);
                assert_eq!(try_read_volatile(&dest).unwrap(), value);
            }
        }

        initialize_try_copy();
        check(0x12u8);
        check(0x3456u16);
        check(0x789abcdeu32);
        check(0x0123456789abcdefu64);
        check(std::array::from_fn::<_, 17, _>(|i| i as u8));
    }

    #[test]
    fn test_copy_and_fill_fault_offsets() {
        let mapping = GuardedMapping::new();
        for len in [1, 8, 32, 1024] {
            let prefix = len / 2;
            let mut buffer = vec![0xa5u8; len];
            // SAFETY: all ranges are reserved; the mapping's middle page is
            // inaccessible and the nonoverlapping buffer is exclusively owned.
            unsafe {
                let crossing = mapping.ptr.add(mapping.page_size - prefix);
                let read = try_copy(crossing, buffer.as_mut_ptr(), len).unwrap_err();
                assert_eq!(read.offset(), prefix);
                assert!(!read.is_write);

                let write = try_copy(buffer.as_ptr(), crossing, len).unwrap_err();
                assert_eq!(write.offset(), prefix);
                assert!(write.is_write);

                for value in [0, 0x5f] {
                    let fill = try_write_bytes(crossing, value, len).unwrap_err();
                    assert_eq!(fill.offset(), prefix);
                    assert!(fill.is_write);
                }
            }
        }
    }

    #[test]
    fn test_public_volatile_and_atomic_faults() {
        let mapping = GuardedMapping::new();
        // SAFETY: the middle page is reserved and naturally aligned, with no
        // Rust references to its inaccessible bytes.
        unsafe {
            let guard = mapping.ptr.add(mapping.page_size);
            let read = try_read_volatile::<u64>(guard.cast()).unwrap_err();
            assert_eq!(read.offset(), 0);
            assert!(!read.is_write);
            let read = try_read_volatile::<[u8; 17]>(guard.cast()).unwrap_err();
            assert_eq!(read.offset(), 0);
            assert!(!read.is_write);

            let write = try_write_volatile(guard.cast(), &0x1234u64).unwrap_err();
            assert_eq!(write.offset(), 0);
            assert!(write.is_write);
            let write = try_write_volatile(guard.cast(), &[0u8; 17]).unwrap_err();
            assert_eq!(write.offset(), 0);
            assert!(write.is_write);

            let atomic = try_compare_exchange(guard.cast::<u64>(), 0, 1).unwrap_err();
            assert_eq!(atomic.offset(), 0);
            assert!(atomic.is_write);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_backward_copy_fault_restores_direction_flag() {
        let mapping = GuardedMapping::new();
        for (len, offset, is_write) in [(3, 2, false), (2, 1, true)] {
            // SAFETY: the overlapping ranges are reserved. Reading RFLAGS
            // preserves it and leaves the stack unchanged.
            unsafe {
                let src = mapping.ptr.add(mapping.page_size - 2);
                let error = try_copy(src, src.add(1), len).unwrap_err();
                let flags: usize;
                core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
                assert_eq!(
                    flags & 0x400,
                    0,
                    "direction flag must be clear after recovery"
                );
                assert_eq!(error.offset(), offset);
                assert_eq!(error.is_write, is_write);
            }
        }
    }

    #[test]
    fn test_public_compare_exchange_sizes() {
        fn check<T: Copy + PartialEq + std::fmt::Debug>(initial: T, new: T, wrong: T) {
            let mut values = [initial; 3];
            // SAFETY: the middle element is a valid, naturally aligned destination.
            unsafe {
                assert_eq!(
                    try_compare_exchange(&mut values[1], initial, new).unwrap(),
                    Ok(new)
                );
                assert_eq!(values, [initial, new, initial]);
                assert_eq!(
                    try_compare_exchange(&mut values[1], wrong, initial).unwrap(),
                    Err(new)
                );
                assert_eq!(values, [initial, new, initial]);
            }
        }

        initialize_try_copy();
        check(0x12u8, 0x34, 0x56);
        check(0x1234u16, 0x5678, 0x9abc);
        check(0x12345678u32, 0x9abcdef0, 0x01234567);
        check(0x0123456789abcdefu64, 0xfedcba9876543210, 0);
    }

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
            is_write: None,
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
