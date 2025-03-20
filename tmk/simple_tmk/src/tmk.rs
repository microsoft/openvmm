// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to write low-level TMK code.
#![expect(unsafe_code)]

mod aarch64;
mod x86;

/// # Safety
///
/// The command must be valid.
unsafe fn command(command: &tmk_protocol::Command) {
    let p = tmk_protocol::COMMAND_ADDRESS as *mut *const tmk_protocol::Command;
    // SAFETY: `p` is known to be a valid pointer.
    unsafe { p.write(command) };
}

fn log_str(msg: &str) {
    // SAFETY: `msg`'s pointer and length are valid.
    unsafe {
        command(&tmk_protocol::Command::Log(tmk_protocol::Str {
            gpa: msg.as_ptr() as u64,
            len: msg.len() as u64,
        }));
    }
}

fn log_fmt(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    let mut s = arrayvec::ArrayString::<1024>::new();
    let _ = s.write_fmt(args);
    log_str(&s);
}

macro_rules! log {
    ($($arg:tt)*) => {
        $crate::tmk::log_fmt(format_args!($($arg)*))
    }
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
fn main() -> ! {
    log!("hello world");
    // SAFETY: the command is valid.
    unsafe { command(&tmk_protocol::Command::Complete { success: true }) };
    panic!("still running?");
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
            message: tmk_protocol::Str {
                gpa: msg.as_ptr() as u64,
                len: msg.len() as u64,
            },
            filename: tmk_protocol::Str {
                gpa: filename.as_ptr() as u64,
                len: filename.len() as u64,
            },
            line,
        });
    }
    minimal_rt::arch::fault();
}
