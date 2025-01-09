// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to launch a graphical terminal for relaying input/output.

use anyhow::Context;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use futures::AsyncWriteExt;
use pal_async::driver::Driver;
use pal_async::local::block_with_io;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use term::raw_stdout;
use term::set_raw_console;
use unix_socket::UnixStream;

/// Synchronously relays stdio to the pipe (Windows) or socket (Unix) pointed to
/// by `path`.
pub fn relay_console(path: &Path) -> anyhow::Result<()> {
    // We use async to read/write to the pipe/socket since on Windows you cannot
    // synchronously read and write to a pipe simultaneously (without overlapped
    // IO).
    //
    // But we use sync to read/write to stdio because it's quite challenging to
    // poll for stdio readiness, especially on Windows. So we use a separate
    // thread for input and output.
    block_with_io(|driver| async move {
        #[cfg(unix)]
        let (read, mut write) = {
            let pipe = PolledSocket::connect_unix(&driver, path)
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
            futures::AsyncReadExt::split(pipe)
        };

        set_raw_console(true);

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

    // Use just the environment variable if specified.
    if let Some(term) = std::env::var_os("OPENVMM_TERM").or_else(|| std::env::var_os("HVLITE_TERM"))
    {
        return vec![App {
            path: PathBuf::from(term).into(),
            args: Vec::new(),
        }];
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
/// hvlite as a child of that to relay the data in the pipe/socket referred to
/// by `path`.
pub fn launch_console(app: Option<&Path>, path: &Path) -> anyhow::Result<()> {
    let apps = choose_terminal_apps(app);

    for app in &apps {
        let mut command = Command::new(app.path.as_ref());
        command.args(&app.args);
        add_argument_separator(&mut command, app.path.as_ref());
        let child = command
            .arg(std::env::current_exe().context("could not determine current exe path")?)
            .arg("--relay-console-path")
            .arg(path)
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
    getrandom::getrandom(&mut random).expect("rng failure");
    path.push(u128::from_ne_bytes(random).to_string());

    path
}

/// Relays a socket to a server usable by [`relay_console`].
///
/// Returns the path of the server, suitable for passing to [`launch_console`].
pub fn relay_console_server(
    driver: &(impl Driver + Spawn + Clone),
    socket: UnixStream,
) -> anyhow::Result<PathBuf> {
    let path = random_console_path();

    #[cfg(windows)]
    let listener = pal_async::windows::pipe::NamedPipeServer::create(&path)
        .context("failed to create pipe server")?;
    #[cfg(unix)]
    let mut listener = PolledSocket::new(
        driver,
        unix_socket::UnixListener::bind(&path).context("failed to bind socket")?,
    )
    .context("failed to create polled listener")?;

    driver
        .spawn("console relay", {
            let driver = driver.clone();
            async move {
                #[cfg(windows)]
                let (pipe_recv, mut pipe_send) = {
                    let pipe = listener
                        .accept(&driver)
                        .context("failed to create pipe")?
                        .await
                        .context("failed to accept connection")?;
                    let pipe = pal_async::pipe::PolledPipe::new(&driver, pipe)
                        .context("failed to create polled pipe")?;

                    futures::AsyncReadExt::split(pipe)
                };

                #[cfg(unix)]
                let (pipe_recv, mut pipe_send) = {
                    let (connection, _) = listener.accept().await.context("failed to accept")?;
                    let connection = PolledSocket::new(&driver, connection)
                        .context("failed to create polled connection")?;
                    connection.split()
                };

                let socket =
                    PolledSocket::new(&driver, socket).context("failed to create polled socket")?;
                let (socket_recv, mut socket_send) = socket.split();

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
        })
        .detach();
    Ok(path)
}
