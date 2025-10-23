// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the execute request.

use futures::executor::block_on;
use futures::io::AllowStdIo;
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
