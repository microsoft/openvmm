// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TMK test framework.

#![no_std]
// UNSAFETY: needed to write low-level TMK code.
#![expect(unsafe_code)]

mod aarch64;
pub mod x86_64;

#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;

use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::null_mut;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering::Relaxed;

/// A TMK test context, passed to each test function.
pub struct TestContext<'scope> {
    /// The BSP VP's scope.
    pub scope: &'scope mut Scope<'scope, 'static>,
}

/// A virtual processor scope, used to interact with the virtual processor
/// in a (relatively) memory safe way.
pub struct Scope<'scope, 'env: 'scope> {
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    arch: arch::ArchScopeState,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl Scope<'_, '_> {
    /// Runs `f` with a subscope, whose effects will be discarded when `f`
    /// returns.
    pub fn subscope<'newenv, R, F>(&mut self, f: F) -> R
    where
        for<'newscope> F: FnOnce(&mut Scope<'newscope, 'newenv>) -> R,
    {
        let mut subscope = Scope {
            arch: Self::arch_init(),
            _scope: PhantomData,
            _env: PhantomData,
        };
        let subscope = &mut subscope;
        let r = f(&mut *subscope);
        subscope.arch_reset();
        r
    }
}

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

/// Logs a message to the TMK log.
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
        $crate::log_fmt(format_args!($($arg)*))
    }
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
fn entry(input: &tmk_protocol::StartInput) -> ! {
    COMMAND_ADDRESS.store(input.command as *mut _, Relaxed);

    // SAFETY: this is the set of test descriptors in the tmk_tests section.
    let tests = unsafe {
        core::slice::from_raw_parts(
            core::ptr::from_ref(&__start_tmk_tests).cast::<TestDescriptor>(),
            (core::ptr::from_ref(&__stop_tmk_tests) as usize
                - core::ptr::from_ref(&__start_tmk_tests) as usize)
                / size_of::<TestDescriptor>(),
        )
    };

    // Find the test to run.
    if input.test_index >= tests.len() as u64 {
        panic!("invalid test index {}", input.test_index);
    }
    let test = &tests[input.test_index as usize];

    let test_name = core::str::from_utf8(test.name).expect("test name in UTF-8");
    log!(
        "running test {test_name}, entrypoint {:#x?}, with input {input:#x?}",
        test.entrypoint
    );

    (test.entrypoint)(TestContext {
        scope: &mut Scope {
            arch: Scope::arch_init(),
            _scope: PhantomData,
            _env: PhantomData,
        },
    });

    log!("test {test_name} completed");

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
            // Strip the crate name from the module path.
            const NAME: &[u8] = const {
                let name = core::concat!(core::module_path!(), "::", $name).as_bytes();
                let mut i = 0usize;
                while name[i] != b':' {
                    i += 1;
                }
                name.split_at(i + 2).1
            };

            // UNSAFETY: needed to specify the link section for the test.
            #[allow(unsafe_code)]
            #[unsafe(link_section = "tmk_tests")]
            #[used]
            static TEST: $crate::TestDescriptor = $crate::TestDescriptor {
                name: NAME,
                entrypoint: $func,
            };
        };
    };
}

/// A TMK test descriptor.
///
/// Has the same layout as [`tmk_protocol::TestDescriptor64`].
#[repr(C)]
pub struct TestDescriptor {
    /// The test name as a UTF-8 string.
    pub name: &'static [u8],
    /// The test entry point.
    pub entrypoint: for<'scope> fn(TestContext<'scope>),
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
