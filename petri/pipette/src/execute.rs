// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the execute request.

// UNSAFETY: Required for libc calls (chroot, chdir, setsid, ioctl) in pre_exec on Linux.
#![cfg_attr(target_os = "linux", expect(unsafe_code))]

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::process::Stdio;

use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn;

pub fn handle_execute(
    driver: &pal_async::DefaultDriver,
    mut request: pipette_protocol::ExecuteRequest,
) -> anyhow::Result<pipette_protocol::ExecuteResponse> {
    tracing::debug!(?request, "execute request");

    let mut command = std::process::Command::new(&request.program);
    command.args(&request.args);
    if let Some(dir) = &request.current_dir {
        command.current_dir(dir);
    }

    // If a chroot is requested, set up a pre_exec hook to chroot the child process.
    if let Some(ref root) = request.chroot {
        #[cfg(target_os = "linux")]
        {
            let root = std::ffi::CString::new(root.as_str())?;
            // SAFETY: calling libc::chroot and libc::chdir in the child process
            // before exec. These are async-signal-safe on Linux.
            unsafe {
                command.pre_exec(move || {
                    if libc::chroot(root.as_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::chdir(c"/".as_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            anyhow::bail!("chroot is only supported on Linux");
        }
    }

    if request.clear_env {
        command.env_clear();
    }
    for pipette_protocol::EnvPair { name, value } in std::mem::take(&mut request.env) {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    // Configure stdio and spawn the child.
    //
    // PTY mode (Linux only): stdin/stdout/stderr go through a PTY slave.
    // Combined stderr: stdout and stderr share an OS pipe.
    // Normal: each stream gets its own pipe.
    #[cfg(target_os = "linux")]
    let pty_master = if request.allocate_pty {
        let (master, slave) = open_pty(&mut command)?;
        command.stdin(Stdio::from(slave.try_clone()?));
        command.stdout(Stdio::from(slave.try_clone()?));
        command.stderr(Stdio::from(slave));
        Some(master)
    } else {
        None
    };

    #[cfg(not(target_os = "linux"))]
    let pty_master: Option<std::fs::File> = if request.allocate_pty {
        anyhow::bail!("PTY allocation is only supported on Linux");
    } else {
        None
    };

    // For combine_stderr, create a pipe and point both stdout and stderr
    // at the write end. The read end becomes the single source for the
    // stdout relay.
    let combined_read =
        if pty_master.is_none() && request.combine_stderr && request.stdout.is_some() {
            let (read_end, write_end) = pal::pipe_pair()?;
            command.stdout(Stdio::from(write_end.try_clone()?));
            command.stderr(Stdio::from(write_end));
            Some(read_end)
        } else {
            None
        };

    // Normal mode: create pipe pairs for each stream, giving one end to
    // the child and keeping the other for an async relay via PolledPipe.
    // This avoids burning a thread per stream.
    let mut stdin_polled = None;
    let mut stdout_polled = None;
    let mut stderr_polled = None;
    if pty_master.is_none() {
        if request.stdin.is_some() {
            let (read_end, write_end) = pal::pipe_pair()?;
            command.stdin(Stdio::from(read_end));
            stdin_polled = Some(PolledPipe::new(driver, write_end)?);
        } else {
            command.stdin(Stdio::null());
        }
        if combined_read.is_none() {
            if request.stdout.is_some() {
                let (read_end, write_end) = pal::pipe_pair()?;
                command.stdout(Stdio::from(write_end));
                stdout_polled = Some(PolledPipe::new(driver, read_end)?);
            } else {
                command.stdout(Stdio::null());
            }
            if request.stderr.is_some() {
                let (read_end, write_end) = pal::pipe_pair()?;
                command.stderr(Stdio::from(write_end));
                stderr_polled = Some(PolledPipe::new(driver, read_end)?);
            } else {
                command.stderr(Stdio::null());
            }
        }
    }

    let child = command.spawn()?;
    let mut polled_child = PolledChild::<std::process::Child>::new(driver, child).unwrap();
    let pid = polled_child.get().id();
    let (send, recv) = mesh::oneshot();

    // Set up I/O relay tasks.
    //
    // PTY mode uses split() which is only available on Unix, so gate it.
    // The pty_master variable is always None on non-Linux.
    #[cfg(target_os = "linux")]
    let pty_relayed = if let Some(master) = pty_master {
        let master = PolledPipe::new(driver, master)?;
        let (master_read, master_write) = master.split();
        if let Some(stdin_pipe) = request.stdin.take() {
            driver
                .spawn("pty stdin relay", relay(stdin_pipe, master_write))
                .detach();
        }
        if let Some(stdout_pipe) = request.stdout.take() {
            driver
                .spawn("pty stdout relay", relay(master_read, stdout_pipe))
                .detach();
        }
        true
    } else {
        false
    };
    #[cfg(not(target_os = "linux"))]
    let pty_relayed = false;

    if !pty_relayed {
        if let (Some(stdin_polled), Some(stdin_pipe)) = (stdin_polled, request.stdin.take()) {
            driver
                .spawn("stdin relay", relay(stdin_pipe, stdin_polled))
                .detach();
        }
        if let Some(read_end) = combined_read {
            let read_end = PolledPipe::new(driver, read_end)?;
            if let Some(stdout_pipe) = request.stdout.take() {
                driver
                    .spawn("combined stdout relay", relay(read_end, stdout_pipe))
                    .detach();
            }
        } else {
            if let (Some(stdout_polled), Some(stdout_pipe)) = (stdout_polled, request.stdout.take())
            {
                driver
                    .spawn("stdout relay", relay(stdout_polled, stdout_pipe))
                    .detach();
            }
            if let (Some(stderr_polled), Some(stderr_pipe)) = (stderr_polled, request.stderr.take())
            {
                driver
                    .spawn("stderr relay", relay(stderr_polled, stderr_pipe))
                    .detach();
            }
        }
    }

    driver
        .spawn("child_wait", async move {
            let exit_status = polled_child.wait().await.unwrap();
            let status = convert_exit_status(exit_status);
            tracing::debug!(pid, ?status, "process exited");
            send.send(status);
        })
        .detach();
    Ok(pipette_protocol::ExecuteResponse { pid, result: recv })
}

async fn relay(
    mut read: impl futures::io::AsyncRead + Unpin,
    mut write: impl futures::io::AsyncWrite + Unpin,
) {
    let _ = futures::io::copy(&mut read, &mut write).await;
}

fn convert_exit_status(exit_status: std::process::ExitStatus) -> pipette_protocol::ExitStatus {
    if let Some(code) = exit_status.code() {
        return pipette_protocol::ExitStatus::Normal(code);
    }

    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&exit_status) {
        return pipette_protocol::ExitStatus::Signal(signal);
    }

    pipette_protocol::ExitStatus::Unknown
}

/// Open a PTY pair and set up a pre_exec hook to create a new session
/// and make the secondary the child's controlling terminal.
#[cfg(target_os = "linux")]
fn open_pty(command: &mut std::process::Command) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    let (primary, secondary) = term::open_pty()?;

    // Create a new session and acquire the controlling terminal. The
    // secondary fd is already on stdin/stdout/stderr (the caller passes it
    // via Stdio::from), so use fd 0 for TIOCSCTTY.
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok((primary, secondary))
}
