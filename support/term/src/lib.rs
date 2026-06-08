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
/// The primary fd has `O_CLOEXEC` set so it is not inherited by child
/// processes. The secondary fd does *not* have `O_CLOEXEC` because it
/// is normally passed to the child as its stdio.
#[cfg(unix)]
pub fn open_pty() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::unix::io::FromRawFd;

    let mut primary_fd = 0;
    let mut secondary_fd = 0;
    // SAFETY: openpty writes to the provided pointers and returns 0 on success.
    if unsafe {
        libc::openpty(
            &mut primary_fd,
            &mut secondary_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both fds are valid from the successful openpty call above.
    let primary = unsafe { std::fs::File::from_raw_fd(primary_fd) };
    // SAFETY: see above.
    let secondary = unsafe { std::fs::File::from_raw_fd(secondary_fd) };

    // Prevent the primary fd from leaking into child processes.
    // openpty() does not set CLOEXEC.
    // SAFETY: primary_fd is valid from the successful openpty call above.
    unsafe {
        let flags = libc::fcntl(primary_fd, libc::F_GETFD);
        libc::fcntl(primary_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }

    Ok((primary, secondary))
}
