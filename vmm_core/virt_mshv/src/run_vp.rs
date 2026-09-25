// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Race-free cancellation of the blocking MSHV_RUN_VP ioctl.
//!
//! A signal delivered before ioctl entry must not be lost. Latch early signals
//! in thread-local storage, and redirect signals in the final check-to-syscall
//! interval to an EINTR return. Once the syscall returns, preserve its result:
//! an intercepted guest instruction must not be discarded.

use hvdef::HvMessage;
use mshv_ioctls::VcpuFd;
use pal::unix::pthread::Pthread;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use zerocopy::FromZeros;

std::thread_local! {
    static CANCELLED: AtomicBool = const { AtomicBool::new(false) };
}

fn signal() -> i32 {
    // KVM owns SIGRTMIN and has a different signal-handler contract.
    libc::SIGRTMIN() + 1
}

pub(super) fn init() -> io::Result<()> {
    static INIT: OnceLock<Result<(), i32>> = OnceLock::new();
    (*INIT.get_or_init(|| {
        // SAFETY: zero is valid for sigaction's C fields. The handler receives
        // a kernel-provided ucontext because SA_SIGINFO is set.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handle_signal as *const () as usize;
        // Preserve restart behavior for unrelated syscalls. A restarted guarded
        // syscall resumes inside the protected interval and is redirected too.
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        // SAFETY: the mask and action are valid for these synchronous C calls.
        let failed = unsafe {
            libc::sigemptyset(&mut action.sa_mask) != 0
                || libc::sigaction(signal(), &action, std::ptr::null_mut()) != 0
        };
        if failed {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .expect("signal setup returned an OS error"))
        } else {
            Ok(())
        }
    }))
    .map_err(io::Error::from_raw_os_error)
}

pub(super) fn prepare_thread() {
    // Initialize TLS before publishing the thread as a cancellation target.
    CANCELLED.with(|_| {});
}

pub(super) fn cancel(thread: Pthread) -> io::Result<()> {
    if thread == Pthread::current() {
        CANCELLED.with(|cancelled| cancelled.store(true, Ordering::Relaxed));
        Ok(())
    } else {
        thread.signal(signal())
    }
}

/// # Safety
/// Must be invoked as a SA_SIGINFO handler with a valid signal context.
unsafe extern "C" fn handle_signal(
    _signal: i32,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    CANCELLED.with(|cancelled| cancelled.store(true, Ordering::Relaxed));
    // SAFETY: SA_SIGINFO supplies a valid, exclusively borrowed signal context.
    let context = unsafe { &mut *context.cast::<libc::ucontext_t>() };
    interrupt_entry(context);
}

fn interrupt_entry(context: &mut libc::ucontext_t) {
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "x86_64")]
    let ip = context.uc_mcontext.gregs[libc::REG_RIP as usize] as usize;
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "aarch64")]
    let ip = context.uc_mcontext.pc as usize;

    let start = guarded_syscall as *const () as usize;
    let end = openvmm_mshv_syscall_return as *const () as usize;
    if (start..end).contains(&ip) {
        // xtask-fmt allow-target-arch sys-crate
        #[cfg(target_arch = "x86_64")]
        {
            context.uc_mcontext.gregs[libc::REG_RIP as usize] = end as _;
            context.uc_mcontext.gregs[libc::REG_RAX as usize] = -i64::from(libc::EINTR);
        }
        // xtask-fmt allow-target-arch sys-crate
        #[cfg(target_arch = "aarch64")]
        {
            context.uc_mcontext.pc = end as _;
            context.uc_mcontext.regs[0] = (-i64::from(libc::EINTR)) as _;
        }
    }
}

unsafe extern "C" {
    fn openvmm_mshv_syscall_return();
}

// These are host syscall ABIs, not guest register layouts. The naked functions
// never change the stack or callee-saved registers, so the signal handler can
// return directly from any instruction before the syscall's return boundary.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
/// # Safety
/// Arguments must satisfy the syscall ABI. The cancellation pointer must refer
/// to a live AtomicBool initialized on the calling thread.
unsafe extern "C" fn guarded_syscall(
    _number: libc::c_long,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _cancelled: *const bool,
) -> libc::c_long {
    core::arch::naked_asm!(
        "mov rax, rdi",
        "mov rdi, rsi",
        "mov rsi, rdx",
        "mov rdx, rcx",
        "cmp byte ptr [r8], 0",
        "jne 2f",
        "syscall",
        ".global openvmm_mshv_syscall_return",
        ".hidden openvmm_mshv_syscall_return",
        "openvmm_mshv_syscall_return:",
        "ret",
        "2:",
        "mov rax, -{eintr}",
        "ret",
        eintr = const libc::EINTR,
    );
}

// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
/// # Safety
/// Arguments must satisfy the syscall ABI. The cancellation pointer must refer
/// to a live AtomicBool initialized on the calling thread.
unsafe extern "C" fn guarded_syscall(
    _number: libc::c_long,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _cancelled: *const bool,
) -> libc::c_long {
    core::arch::naked_asm!(
        "mov x8, x0",
        "mov x0, x1",
        "mov x1, x2",
        "mov x2, x3",
        "ldarb w9, [x4]",
        "cbnz w9, 2f",
        "svc #0",
        ".global openvmm_mshv_syscall_return",
        ".hidden openvmm_mshv_syscall_return",
        "openvmm_mshv_syscall_return:",
        "ret",
        "2:",
        "mov x0, #-{eintr}",
        "ret",
        eintr = const libc::EINTR,
    );
}

/// # Safety
/// The caller must provide valid arguments and pointers for the syscall.
unsafe fn syscall(number: libc::c_long, args: [usize; 3]) -> io::Result<libc::c_long> {
    CANCELLED.with(|cancelled| {
        if cancelled.swap(false, Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::EINTR));
        }
        // SAFETY: the caller provides the syscall arguments; the cancellation
        // flag is live and is read atomically by the guarded assembly.
        let result =
            unsafe { guarded_syscall(number, args[0], args[1], args[2], cancelled.as_ptr()) };
        if result < 0 {
            Err(io::Error::from_raw_os_error(-result as i32))
        } else {
            Ok(result)
        }
    })
}

pub(super) fn run(vcpu: &VcpuFd) -> io::Result<HvMessage> {
    const {
        assert!(size_of::<HvMessage>() == size_of::<mshv_bindings::mshv_run_vp>());
    }
    let mut message = HvMessage::new_zeroed();
    // SAFETY: the borrowed VP fd remains open, and the kernel may write one
    // complete intercept message to this live, correctly sized output buffer.
    let result = unsafe {
        syscall(
            libc::SYS_ioctl,
            [
                vcpu.as_raw_fd() as usize,
                mshv_ioctls::MSHV_RUN_VP() as usize,
                std::ptr::from_mut(&mut message) as usize,
            ],
        )?
    };
    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("MSHV_RUN_VP returned unexpected result {result}"),
        ));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;

    fn initialize() {
        init().unwrap();
        prepare_thread();
        CANCELLED.with(|cancelled| cancelled.store(false, Ordering::Relaxed));
    }

    fn getpid() -> io::Result<libc::c_long> {
        // SAFETY: getpid has no pointer arguments or memory side effects.
        unsafe { syscall(libc::SYS_getpid, [0; 3]) }
    }

    #[test]
    fn signal_before_entry_is_latched() {
        initialize();
        Pthread::current().signal(signal()).unwrap();
        assert_eq!(getpid().unwrap_err().raw_os_error(), Some(libc::EINTR));
        assert_eq!(getpid().unwrap(), i64::from(std::process::id()));
    }

    #[test]
    fn same_thread_cancellation_is_latched() {
        initialize();
        cancel(Pthread::current()).unwrap();
        assert_eq!(getpid().unwrap_err().raw_os_error(), Some(libc::EINTR));
        assert_eq!(getpid().unwrap(), i64::from(std::process::id()));
    }

    #[test]
    fn syscall_errors_are_preserved() {
        initialize();
        // SAFETY: -1 is an invalid fd; close cannot release a live descriptor.
        let result = unsafe { syscall(libc::SYS_close, [usize::MAX, 0, 0]) };
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn entry_redirect_preserves_completed_syscalls() {
        let start = guarded_syscall as *const () as usize;
        let end = openvmm_mshv_syscall_return as *const () as usize;
        assert!(start < end);
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "x86_64")]
        let restarted_syscall_ip = end - 2;
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "aarch64")]
        let restarted_syscall_ip = end - 4;
        for (ip, interrupted) in [
            (start - 1, false),
            (start, true),
            (restarted_syscall_ip, true),
            (end - 1, true),
            (end, false),
            (end + 1, false),
        ] {
            // SAFETY: zero is valid for this C structure; the test accesses
            // only its integer instruction-pointer and return-value fields.
            let mut context: libc::ucontext_t = unsafe { std::mem::zeroed() };
            // xtask-fmt allow-target-arch sys-crate
            #[cfg(target_arch = "x86_64")]
            {
                context.uc_mcontext.gregs[libc::REG_RIP as usize] = ip as _;
                context.uc_mcontext.gregs[libc::REG_RAX as usize] = 123;
            }
            // xtask-fmt allow-target-arch sys-crate
            #[cfg(target_arch = "aarch64")]
            {
                context.uc_mcontext.pc = ip as _;
                context.uc_mcontext.regs[0] = 123;
            }
            interrupt_entry(&mut context);
            let expected_ip = if interrupted { end } else { ip };
            let expected_result = if interrupted {
                -i64::from(libc::EINTR)
            } else {
                123
            };
            // xtask-fmt allow-target-arch sys-crate
            #[cfg(target_arch = "x86_64")]
            {
                assert_eq!(
                    context.uc_mcontext.gregs[libc::REG_RIP as usize] as usize,
                    expected_ip
                );
                assert_eq!(
                    context.uc_mcontext.gregs[libc::REG_RAX as usize],
                    expected_result
                );
            }
            // xtask-fmt allow-target-arch sys-crate
            #[cfg(target_arch = "aarch64")]
            {
                assert_eq!(context.uc_mcontext.pc as usize, expected_ip);
                assert_eq!(context.uc_mcontext.regs[0] as i64, expected_result);
            }
        }
    }

    #[test]
    fn signal_interrupts_blocking_syscall() {
        init().unwrap();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (ready_send, ready_recv) = mpsc::channel();
        let (done_send, done_recv) = mpsc::channel();
        let task = std::thread::spawn(move || {
            initialize();
            ready_send.send(Pthread::current()).unwrap();
            let mut byte = 0u8;
            // SAFETY: the socket stays open and the byte is a live output
            // buffer for a one-byte read throughout the blocking syscall.
            let result = unsafe {
                syscall(
                    libc::SYS_read,
                    [
                        reader.as_raw_fd() as usize,
                        std::ptr::from_mut(&mut byte) as usize,
                        1,
                    ],
                )
            };
            done_send.send(result).unwrap();
        });
        cancel(ready_recv.recv().unwrap()).unwrap();
        let result = done_recv.recv_timeout(Duration::from_secs(2));
        if matches!(result, Err(mpsc::RecvTimeoutError::Timeout)) {
            // Release a regressed read before failing instead of leaking a
            // blocked test thread.
            writer.write_all(&[0]).unwrap();
        }
        task.join().unwrap();
        assert_eq!(
            result
                .expect("cancellation did not interrupt read")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINTR)
        );
    }
}
