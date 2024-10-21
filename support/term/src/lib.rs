// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functionality to assist with managing the terminal/console/tty.

#![warn(missing_docs)]
// UNSAFETY: Win32 and libc function calls to manipulate terminal state.
#![allow(unsafe_code)]

/// Enables VT and UTF-8 output.
#[cfg(windows)]
pub fn enable_vt_and_utf8() {
    use winapi::um::consoleapi;
    use winapi::um::processenv;
    use winapi::um::winbase;
    use winapi::um::wincon;
    use winapi::um::winnls;
    // SAFETY: calling Windows APIs as documented.
    unsafe {
        let conout = processenv::GetStdHandle(winbase::STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if consoleapi::GetConsoleMode(conout, &mut mode) != 0 {
            if mode & wincon::ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0 {
                consoleapi::SetConsoleMode(
                    conout,
                    mode | wincon::ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                );
            }
            wincon::SetConsoleOutputCP(winnls::CP_UTF8);
        }
    }
}

/// Enables VT and UTF-8 output. No-op on non-Windows platforms.
#[cfg(not(windows))]
pub fn enable_vt_and_utf8() {}

/// Enables or disables raw console mode.
#[cfg(windows)]
pub fn set_raw_console(enable: bool) {
    use winapi::shared::minwindef;
    use winapi::um::consoleapi;
    use winapi::um::processenv;
    use winapi::um::winbase;
    use winapi::um::wincon;
    use winapi::um::winnls;

    // SAFETY: calling Windows APIs as documented.
    unsafe {
        let conin = processenv::GetStdHandle(winbase::STD_INPUT_HANDLE);
        let mut mode: minwindef::DWORD = 0;
        if consoleapi::GetConsoleMode(conin, &mut mode) != 0 {
            let on = wincon::ENABLE_VIRTUAL_TERMINAL_INPUT;
            let off = wincon::ENABLE_LINE_INPUT
                | wincon::ENABLE_ECHO_INPUT
                | wincon::ENABLE_PROCESSED_INPUT;
            if enable {
                mode |= on;
                mode &= !off;
            } else {
                mode &= !on;
                mode |= off;
            }
            consoleapi::SetConsoleMode(conin, mode);
        }
        let conout = processenv::GetStdHandle(winbase::STD_OUTPUT_HANDLE);
        if consoleapi::GetConsoleMode(conout, &mut mode) != 0 {
            let on =
                wincon::ENABLE_VIRTUAL_TERMINAL_PROCESSING | wincon::DISABLE_NEWLINE_AUTO_RETURN;
            let off = 0;
            if enable {
                mode |= on;
                mode &= !off;
            } else {
                mode &= !on;
                mode |= off;
            }
            consoleapi::SetConsoleMode(conout, mode);
            wincon::SetConsoleOutputCP(winnls::CP_UTF8);
        }
    }
}

/// Enables or disables raw console mode.
#[cfg(not(windows))]
pub fn set_raw_console(enable: bool) {
    if enable {
        crossterm::terminal::enable_raw_mode().unwrap();
    } else {
        crossterm::terminal::disable_raw_mode().unwrap();
    }
}

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
