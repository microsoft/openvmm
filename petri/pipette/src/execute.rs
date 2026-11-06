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
    for pipette_protocol::EnvPair { name, value } in request.env {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    // Configure stdio, set up relay tasks, then spawn the child.
    //
    // Relay tasks are started before spawn — they simply block until the
    // child produces/consumes data. This keeps all fallible setup before
    // spawn() so a failure can't leak a running child process.
    //
    // PTY mode (Linux only): stdin/stdout/stderr go through a PTY secondary.
    // Combined stderr: stdout and stderr share an OS pipe.
    // Normal: each stream gets its own pipe.
    if request.allocate_pty {
        #[cfg(target_os = "linux")]
        {
            let (primary, secondary) = term::open_pty()?;
            command.stdin(Stdio::from(secondary.try_clone()?));
            command.stdout(Stdio::from(secondary.try_clone()?));
            command.stderr(Stdio::from(secondary));

            // Create a new session and acquire the controlling terminal.
            // The secondary fd is on stdin/stdout/stderr, so use fd 0 for TIOCSCTTY.
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
            let primary = PolledPipe::new(driver, primary)?;
            let (primary_read, primary_write) = primary.split();
            if let Some(stdin_pipe) = request.stdin.take() {
                driver
                    .spawn("pty_stdin_relay", relay(stdin_pipe, primary_write))
                    .detach();
            }
            if let Some(stdout_pipe) = request.stdout.take() {
                driver
                    .spawn("pty_stdout_relay", relay(primary_read, stdout_pipe))
                    .detach();
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("PTY allocation is only supported on Linux");
        }
    } else {
        if let Some(stdin_pipe) = request.stdin.take() {
            let (read_end, write_end) = pal::pipe_pair()?;
            command.stdin(Stdio::from(read_end));
            let write_end = PolledPipe::new(driver, write_end)?;
            driver
                .spawn("stdin_relay", relay(stdin_pipe, write_end))
                .detach();
        } else {
            command.stdin(Stdio::null());
        }

        if request.combine_stderr {
            if let Some(stdout_pipe) = request.stdout.take() {
                let (read_end, write_end) = pal::pipe_pair()?;
                command.stdout(Stdio::from(write_end.try_clone()?));
                command.stderr(Stdio::from(write_end));
                let read_end = PolledPipe::new(driver, read_end)?;
                driver
                    .spawn("combined_stdout_relay", relay(read_end, stdout_pipe))
                    .detach();
            } else {
                command.stdout(Stdio::null());
                command.stderr(Stdio::null());
            }
        } else {
            if let Some(stdout_pipe) = request.stdout.take() {
                let (read_end, write_end) = pal::pipe_pair()?;
                command.stdout(Stdio::from(write_end));
                let read_end = PolledPipe::new(driver, read_end)?;
                driver
                    .spawn("stdout_relay", relay(read_end, stdout_pipe))
                    .detach();
            } else {
                command.stdout(Stdio::null());
            }
            if let Some(stderr_pipe) = request.stderr.take() {
                let (read_end, write_end) = pal::pipe_pair()?;
                command.stderr(Stdio::from(write_end));
                let read_end = PolledPipe::new(driver, read_end)?;
                driver
                    .spawn("stderr_relay", relay(read_end, stderr_pipe))
                    .detach();
            } else {
                command.stderr(Stdio::null());
            }
        }
    }

    let child = command.spawn()?;
    let mut polled_child = PolledChild::<std::process::Child>::new(driver, child)
        .expect("process was just spawned, driver must be able to wait on it");
    let pid = polled_child.get().id();
    let (send, recv) = mesh::oneshot();

    driver
        .spawn("child_wait", async move {
            let exit_status = polled_child
                .wait()
                .await
                .expect("waiting on a spawned child should not fail");
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
