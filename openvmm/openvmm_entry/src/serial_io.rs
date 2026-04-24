// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cleanup_socket;
use anyhow::Context;
use pal_async::driver::Driver;
#[cfg(windows)]
use pal_async::pipe::PolledPipe;
use serial_socket::net::OpenSocketSerialConfig;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

#[cfg(unix)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(
    Resource<SerialBackendHandle>,
    pal_async::socket::PolledSocket<unix_socket::UnixStream>,
)> {
    let (left, right) = unix_socket::UnixStream::pair()?;
    let right = pal_async::socket::PolledSocket::new(driver, right)?;
    Ok((OpenSocketSerialConfig::from(left).into_resource(), right))
}

#[cfg(windows)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(Resource<SerialBackendHandle>, PolledPipe)> {
    use serial_socket::windows::OpenWindowsPipeSerialConfig;

    // Use named pipes on Windows even though we also support Unix sockets
    // there. This avoids an unnecessary winsock dependency.
    let (server, client) = pal::windows::pipe::bidirectional_pair(false)?;
    let server = PolledPipe::new(driver, server)?;
    // Use the client for the VM side so that it does not try to reconnect
    // (which isn't possible via pal_async for pipes opened in non-overlapped
    // mode, anyway).
    Ok((
        OpenWindowsPipeSerialConfig::from(client).into_resource(),
        server,
    ))
}

pub fn bind_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = pal::windows::pipe::new_named_pipe(
                path,
                windows_sys::Win32::Foundation::GENERIC_READ
                    | windows_sys::Win32::Foundation::GENERIC_WRITE,
                pal::windows::pipe::Disposition::Create,
                pal::windows::pipe::PipeMode::Byte,
            )?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    cleanup_socket(path);
    Ok(OpenSocketSerialConfig::from(UnixListener::bind(path)?).into_resource())
}

pub fn bind_tcp_serial(addr: &SocketAddr) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("failed to bind tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}
