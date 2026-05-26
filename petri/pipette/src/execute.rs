// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the execute request.

// UNSAFETY: Required for libc::chroot() and libc::chdir() in pre_exec on Linux.
#![cfg_attr(target_os = "linux", expect(unsafe_code))]

use futures::executor::block_on;
use futures::io::AllowStdIo;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::process::Stdio;

pub fn handle_execute(
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
    if request.stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    if request.stdout.is_some() {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    if request.stderr.is_some() {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::null());
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let (send, recv) = mesh::oneshot();

    if let (Some(stdin_write), Some(stdin_read)) = (child.stdin.take(), request.stdin.take()) {
        std::thread::spawn(move || {
            let _ = block_on(futures::io::copy(
                stdin_read,
                &mut AllowStdIo::new(stdin_write),
            ));
        });
    }
    if let (Some(stdout_read), Some(mut stdout_write)) =
        (child.stdout.take(), request.stdout.take())
    {
        std::thread::spawn(move || {
            let _ = block_on(futures::io::copy(
                AllowStdIo::new(stdout_read),
                &mut stdout_write,
            ));
        });
    }
    if let (Some(stderr_read), Some(mut stderr_write)) =
        (child.stderr.take(), request.stderr.take())
    {
        std::thread::spawn(move || {
            let _ = block_on(futures::io::copy(
                AllowStdIo::new(stderr_read),
                &mut stderr_write,
            ));
        });
    }

    std::thread::spawn(move || {
        let exit_status = child.wait().unwrap();
        let status = convert_exit_status(exit_status);
        tracing::debug!(pid, ?status, "process exited");
        send.send(status);
    });
    Ok(pipette_protocol::ExecuteResponse { pid, result: recv })
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
