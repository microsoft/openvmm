// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to launch a terminal emulator for relaying input/output.

#![forbid(unsafe_code)]

mod unix;
mod windows;

use anyhow::Context as _;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use futures::io::AsyncReadExt;
use pal_async::driver::Driver;
use pal_async::local::block_with_io;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::task::Context;
use term::raw_stdout;
use term::set_console_title;
use term::set_raw_console;

#[derive(Default)]
/// Options to configure a new console window during launch.
pub struct ConsoleLaunchOptions {
    /// If supplied, sets the title of the console window.
    pub window_title: Option<String>,
}

/// Synchronously relays stdio to the pipe (Windows) or socket (Unix) pointed to
/// by `path`.
pub fn relay_console(path: &Path, console_title: &str) -> anyhow::Result<()> {
    // We use async to read/write to the pipe/socket since on Windows you cannot
    // synchronously read and write to a pipe simultaneously (without overlapped
    // IO).
    //
    // But we use sync to read/write to stdio because it's quite challenging to
    // poll for stdio readiness, especially on Windows. So we use a separate
    // thread for input and output.
    block_with_io(async |driver| {
        #[cfg(unix)]
        let (read, mut write) = {
            let pipe = pal_async::socket::PolledSocket::connect_unix(&driver, path)
                .await
                .context("failed to connect to console socket")?;
            pipe.split()
        };
        #[cfg(windows)]
        let (read, mut write) = {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .context("failed to connect to console pipe")?;
            let pipe = pal_async::pipe::PolledPipe::new(&driver, pipe)
                .context("failed to create polled pipe")?;
            AsyncReadExt::split(pipe)
        };

        set_raw_console(true).expect("failed to set raw console mode");
        if let Err(err) = set_console_title(console_title) {
            tracing::warn!("failed to set console title: {}", err);
        }

        std::thread::Builder::new()
            .name("input_thread".into())
            .spawn({
                move || {
                    block_on(futures::io::copy(
                        AllowStdIo::new(std::io::stdin()),
                        &mut write,
                    ))
                }
            })
            .unwrap();

        futures::io::copy(read, &mut AllowStdIo::new(raw_stdout())).await?;
        // Don't wait for the input thread, since it is probably blocking in the stdin read.
        Ok(())
    })
}

struct App<'a> {
    path: Cow<'a, Path>,
    args: Vec<Cow<'a, OsStr>>,
}

impl<'a, T: AsRef<OsStr> + ?Sized> From<&'a T> for App<'a> {
    fn from(value: &'a T) -> Self {
        Self {
            path: Path::new(value).into(),
            args: Vec::new(),
        }
    }
}

fn choose_terminal_apps(app: Option<&Path>) -> Vec<App<'_>> {
    // If a specific app was specified, use it with no fallbacks.
    if let Some(app) = app {
        return vec![app.into()];
    }

    let mut apps = Vec::new();

    let env_set = |key| std::env::var_os(key).is_some_and(|x| !x.is_empty());

    // If we're running in tmux, use tmux.
    if env_set("TMUX") {
        apps.push(App {
            args: vec![OsStr::new("new-window").into()],
            .."tmux".into()
        });
    }

    // If there's an X11 display, use x-terminal-emulator or xterm.
    if cfg!(unix) && env_set("DISPLAY") {
        apps.push("x-terminal-emulator".into());
        apps.push("xterm".into());
    }

    // On Windows, use Windows Terminal or conhost.
    if cfg!(windows) {
        apps.push("wt.exe".into());
        apps.push("conhost.exe".into());
    }

    apps
}

/// Launches the terminal application `app` (or the system default), and launch
/// OpenVMM as a child of that to relay the data in the pipe/socket referred to
/// by `path`. Additional launch options can be specified with `launch_options`.
pub fn launch_console(
    app: Option<&Path>,
    path: &Path,
    launch_options: ConsoleLaunchOptions,
) -> anyhow::Result<()> {
    let apps = choose_terminal_apps(app);

    for app in &apps {
        let mut command = Command::new(app.path.as_ref());
        command.args(&app.args);
        add_argument_separator(&mut command, app.path.as_ref());
        let mut child_builder = command
            .arg(std::env::current_exe().context("could not determine current exe path")?)
            .arg("--relay-console-path")
            .arg(path);

        // If a title was specified, pass it to the terminal spawn.
        if let Some(title) = &launch_options.window_title {
            child_builder = child_builder.arg("--relay-console-title").arg(title);
        }

        let child = child_builder
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn();

        match child {
            Ok(mut child) => {
                std::thread::Builder::new()
                    .name("console_waiter".into())
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .unwrap();

                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && apps.len() != 1 => continue,
            Err(err) => Err(err)
                .with_context(|| format!("failed to launch terminal {}", app.path.display()))?,
        };
    }

    anyhow::bail!("could not find a terminal emulator");
}

/// Adds the terminal-specific separator between terminal arguments and the
/// process to launch.
fn add_argument_separator(command: &mut Command, app: &Path) {
    if let Some(file_name) = app.file_name().and_then(|s| s.to_str()) {
        let arg = match file_name {
            "xterm" | "rxvt" | "urxvt" | "x-terminal-emulator" => "-e",
            _ => "--",
        };
        command.arg(arg);
    };
}

/// Computes a random console path (pipe path for Windows, Unix socket path for Unix).
pub fn random_console_path() -> PathBuf {
    #[cfg(windows)]
    let mut path = PathBuf::from("\\\\.\\pipe");
    #[cfg(unix)]
    let mut path = std::env::temp_dir();

    let mut random = [0; 16];
    getrandom::fill(&mut random).expect("rng failure");
    path.push(u128::from_ne_bytes(random).to_string());

    path
}

/// An external console window.
///
/// To write to the console, use methods from [`AsyncWrite`]. To read from the
/// console, use methods from [`AsyncRead`].
pub struct Console {
    #[cfg(windows)]
    sys: windows::WindowsNamedPipeConsole,
    #[cfg(unix)]
    sys: unix::UnixSocketConsole,
}

impl Console {
    /// Launches a new terminal emulator and returns an object used to
    /// read/write to the console of that window.
    ///
    /// If `app` is `None`, the system default terminal emulator is used.
    ///
    /// The terminal emulator will relaunch the current executable with the
    /// `--relay-console-path` argument to specify the path of the pipe/socket
    /// used to relay data. Call [`relay_console`] with that path in your `main`
    /// function.
    pub fn new(
        driver: impl Driver,
        app: Option<&Path>,
        launch_options: Option<ConsoleLaunchOptions>,
    ) -> anyhow::Result<Self> {
        let path = random_console_path();
        let this = Self::new_from_path(driver, &path)?;
        launch_console(app, &path, launch_options.unwrap_or_default())
            .context("failed to launch console")?;
        Ok(this)
    }

    fn new_from_path(driver: impl Driver, path: &Path) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let sys = windows::WindowsNamedPipeConsole::new(Box::new(driver), path)
            .context("failed to create console pipe")?;
        #[cfg(unix)]
        let sys = unix::UnixSocketConsole::new(Box::new(driver), path)
            .context("failed to create console socket")?;
        Ok(Console { sys })
    }

    /// Relays the console contents to and from `io`.
    pub async fn relay(&mut self, io: impl AsyncRead + AsyncWrite) -> anyhow::Result<()> {
        let (pipe_recv, mut pipe_send) = { AsyncReadExt::split(self) };

        let (socket_recv, mut socket_send) = io.split();

        let task_a = async move {
            let r = futures::io::copy(pipe_recv, &mut socket_send).await;
            let _ = socket_send.close().await;
            r
        };
        let task_b = async move {
            let r = futures::io::copy(socket_recv, &mut pipe_send).await;
            let _ = pipe_send.close().await;
            r
        };
        futures::future::try_join(task_a, task_b).await?;
        anyhow::Result::<_>::Ok(())
    }
}

impl AsyncRead for Console {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().sys).poll_read(cx, buf)
    }
}

impl AsyncWrite for Console {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().sys).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().sys).poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().sys).poll_close(cx)
    }
}
