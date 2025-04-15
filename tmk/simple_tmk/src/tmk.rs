// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to write low-level TMK code.
#![expect(unsafe_code)]

use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering::Relaxed;

mod aarch64;
mod x86;

static COMMAND_ADDRESS: AtomicPtr<*const tmk_protocol::Command> = AtomicPtr::new(null_mut());

/// # Safety
///
/// The command must be valid.
unsafe fn command(command: &tmk_protocol::Command) {
    let p = COMMAND_ADDRESS.load(Relaxed);
    // SAFETY: `p` is known to be a valid pointer.
    unsafe { p.write(command) };
}

fn log_str(msg: &str) {
    // SAFETY: `msg`'s pointer and length are valid.
    unsafe {
        command(&tmk_protocol::Command::Log(tmk_protocol::StrDescriptor {
            gpa: msg.as_ptr() as u64,
            len: msg.len() as u64,
        }));
    }
}

pub fn log_fmt(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    let mut s = arrayvec::ArrayString::<1024>::new();
    let _ = s.write_fmt(args);
    log_str(&s);
}

/// Logs a message to the TMK log.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::tmk::log_fmt(format_args!($($arg)*))
    }
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
fn main(input: &tmk_protocol::StartInput) -> ! {
    COMMAND_ADDRESS.store(input.command as *mut _, Relaxed);

    // SAFETY: this is the set of test descriptors in the tmk_tests section.
    let tests = unsafe {
        core::slice::from_raw_parts(
            core::ptr::from_ref(&__start_tmk_tests).cast::<tmk_protocol::TestDescriptor>(),
            (core::ptr::from_ref(&__stop_tmk_tests) as usize
                - core::ptr::from_ref(&__start_tmk_tests) as usize)
                / size_of::<tmk_protocol::TestDescriptor>(),
        )
    };

    // Find the test to run.
    if input.test_index >= tests.len() as u64 {
        panic!("invalid test index {}", input.test_index);
    }
    let test = &tests[input.test_index as usize];
    (test.entrypoint)();

    // SAFETY: the command is valid.
    unsafe { command(&tmk_protocol::Command::Complete { success: true }) };
    panic!("still running?");
}

unsafe extern "C" {
    safe static __start_tmk_tests: c_void;
    safe static __stop_tmk_tests: c_void;
}

/// Used internally by [`tmk_test`] to define a task in a way that can be parsed
/// from the ELF binary by the TMK loader.
#[doc(hidden)]
#[macro_export]
macro_rules! define_tmk_test {
    ($name:expr, $func:ident) => {
        const _: () = {
            // UNSAFETY: needed to specify the link section for the test.
            #[allow(unsafe_code)]
            #[unsafe(link_section = "tmk_tests")]
            #[used]
            static TEST: tmk_protocol::TestDescriptor = tmk_protocol::TestDescriptor {
                name: $name,
                entrypoint: $func,
            };
        };
    };
}

#[cfg_attr(minimal_rt, panic_handler)]
#[cfg_attr(not(minimal_rt), expect(dead_code))]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write;
    let mut msg = arrayvec::ArrayString::<1024>::new();
    let _ = write!(&mut msg, "{}", info.message());
    let (filename, line) = info.location().map_or(("", 0), |l| (l.file(), l.line()));
    // SAFETY: the command is valid.
    unsafe {
        command(&tmk_protocol::Command::Panic {
            message: tmk_protocol::StrDescriptor {
                gpa: msg.as_ptr() as u64,
                len: msg.len() as u64,
            },
            filename: tmk_protocol::StrDescriptor {
                gpa: filename.as_ptr() as u64,
                len: filename.len() as u64,
            },
            line,
        });
    }
    minimal_rt::arch::fault();
}
