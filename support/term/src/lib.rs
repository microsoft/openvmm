// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functionality to assist with managing the terminal/console/tty.

// UNSAFETY: Win32 and libc function calls to manipulate terminal state.
#![expect(unsafe_code)]

/// Enables VT and UTF-8 output.
#[cfg(windows)]
pub fn enable_vt_and_utf8() {
    use windows_sys::Win32::Globalization::CP_UTF8;
    use windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
    use windows_sys::Win32::System::Console::SetConsoleMode;
    use windows_sys::Win32::System::Console::SetConsoleOutputCP;
    // SAFETY: calling Windows APIs as documented.
    unsafe {
        let conout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(conout, &mut mode) != 0 {
            if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0 {
                SetConsoleMode(conout, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
            SetConsoleOutputCP(CP_UTF8);
        }
    }
}

/// Enables VT and UTF-8 output. No-op on non-Windows platforms.
#[cfg(not(windows))]
pub fn enable_vt_and_utf8() {}

/// Clones `file` into a `File`.
///
/// # Safety
/// The caller must ensure `file` owns a valid file.
#[cfg(windows)]
fn clone_file(file: impl std::os::windows::io::AsHandle) -> std::fs::File {
    file.as_handle().try_clone_to_owned().unwrap().into()
}

/// Clones `file` into a `File`.
///
/// # Safety
/// The caller must ensure `file` owns a valid file.
#[cfg(unix)]
fn clone_file(file: impl std::os::unix::io::AsFd) -> std::fs::File {
    file.as_fd().try_clone_to_owned().unwrap().into()
}

/// Returns a non-buffering stdout, with no special console handling on Windows.
pub fn raw_stdout() -> std::fs::File {
    clone_file(std::io::stdout())
}

/// Returns a non-buffering stderr, with no special console handling on Windows.
pub fn raw_stderr() -> std::fs::File {
    clone_file(std::io::stderr())
}

/// Sets a panic handler to restore the terminal state when the process panics.
#[cfg(unix)]
pub fn revert_terminal_on_panic() {
    let orig_termios = get_termios();

    let base_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("restoring terminal attributes on panic...");
        set_termios(orig_termios);
        base_hook(info)
    }));
}

/// Opaque wrapper around `libc::termios`.
#[cfg(unix)]
#[derive(Copy, Clone)]
pub struct Termios(libc::termios);

/// Get the current termios settings for stderr.
#[cfg(unix)]
pub fn get_termios() -> Termios {
    let mut orig_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` has no preconditions, and stderr has been checked to be a tty
    let ret = unsafe { libc::tcgetattr(libc::STDERR_FILENO, orig_termios.as_mut_ptr()) };
    if ret != 0 {
        panic!(
            "error: could not save term attributes: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: `tcgetattr` returned successfully, therefore `orig_termios` has been initialized
    let orig_termios = unsafe { orig_termios.assume_init() };
    Termios(orig_termios)
}

/// Set the termios settings for stderr.
#[cfg(unix)]
pub fn set_termios(termios: Termios) {
    // SAFETY: stderr is guaranteed to be an open fd, and `termios` is a valid termios struct.
    let ret = unsafe { libc::tcsetattr(libc::STDERR_FILENO, libc::TCSAFLUSH, &termios.0) };
    if ret != 0 {
        panic!(
            "error: could not restore term attributes via tcsetattr: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Opens a PTY pair, returning `(primary, secondary)`.
///
/// Both fds have `O_CLOEXEC` set atomically at open time so they
/// cannot leak into child processes even under concurrent `fork`.
/// Callers that need the secondary in a child should pass it via
/// `Stdio::from()`, which `dup2`s it onto stdin/stdout/stderr.
#[cfg(unix)]
pub fn open_pty() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::ffi::CStr;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::FromRawFd;

    // Use the POSIX flow (posix_openpt + grantpt + unlockpt + open)
    // instead of openpty() so we can pass O_CLOEXEC at open time,
    // avoiding a race between open and fcntl.

    // SAFETY: posix_openpt is called with valid flags.
    let primary_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if primary_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: primary_fd is valid from the successful posix_openpt call.
    let primary = unsafe { std::fs::File::from_raw_fd(primary_fd) };

    // SAFETY: the fd is valid. grantpt/unlockpt have no preconditions
    // beyond a valid primary fd.
    unsafe {
        if libc::grantpt(primary.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::unlockpt(primary.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    // ptsname_r is missing from libc for macos, despite being present, and
    // it's very hard to get improvements upstream. So, just define it here.
    #[cfg(not(target_os = "macos"))]
    use libc::ptsname_r;
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        unsafe fn ptsname_r(fd: i32, buf: *mut std::ffi::c_char, buflen: usize) -> i32;
    }

    // Get the secondary device name using ptsname_r (thread-safe).
    let mut name_buf = [0u8; 128];
    // SAFETY: ptsname_r writes into the provided buffer and null-terminates.
    let ret = unsafe {
        ptsname_r(
            primary.as_raw_fd(),
            name_buf.as_mut_ptr().cast(),
            name_buf.len(),
        )
    };
    if ret != 0 {
        return Err(std::io::Error::from_raw_os_error(ret));
    }

    let name = CStr::from_bytes_until_nul(&name_buf).expect("libc contract violation");
    let secondary = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(OsStr::from_bytes(name.to_bytes()))?;

    Ok((primary, secondary))
}
