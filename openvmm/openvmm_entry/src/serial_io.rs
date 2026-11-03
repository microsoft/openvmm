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

pub(crate) mod connect;
pub mod microvm;

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

/// Returns an anonymous serial pair, as [`anonymous_serial_pair`] does, and a
/// non-blocking duplicate of its host end. The duplicate reads what the pair
/// holds without waiting for the reactor to report it, and then reports that
/// the pair is empty.
pub(crate) fn anonymous_serial_pair_with_pending(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(Resource<SerialBackendHandle>, PolledHostEnd, PendingHostEnd)> {
    let (vm, host, pending) = pending_pair(driver)?;
    #[cfg(unix)]
    let vm = OpenSocketSerialConfig::from(vm).into_resource();
    #[cfg(windows)]
    let vm = serial_socket::windows::OpenWindowsPipeSerialConfig::from(vm).into_resource();
    Ok((vm, host, pending))
}

#[cfg(unix)]
pub(crate) type PolledHostEnd = pal_async::socket::PolledSocket<unix_socket::UnixStream>;
#[cfg(unix)]
pub(crate) type PendingHostEnd = unix_socket::UnixStream;
#[cfg(windows)]
pub(crate) type PolledHostEnd = PolledPipe;
#[cfg(windows)]
pub(crate) type PendingHostEnd = std::fs::File;

/// Returns the VM end of an anonymous pair, its polled host end, and a
/// non-blocking duplicate of the host end.
#[cfg(unix)]
pub(crate) fn pending_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(unix_socket::UnixStream, PolledHostEnd, PendingHostEnd)> {
    let (vm, host) = unix_socket::UnixStream::pair()?;
    let pending = host.try_clone()?;
    let host = pal_async::socket::PolledSocket::new(driver, host)?;
    pending.set_nonblocking(true)?;
    Ok((vm, host, pending))
}

/// Returns the VM end of an anonymous pair, its polled host end, and a
/// non-blocking duplicate of the host end.
#[cfg(windows)]
pub(crate) fn pending_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(std::fs::File, PolledHostEnd, PendingHostEnd)> {
    use pal::windows::pipe::PipeExt;

    // As in anonymous_serial_pair, the VM takes the client.
    let (server, client) = pal::windows::pipe::bidirectional_pair(false)?;
    let pending = server.try_clone()?;
    let server = PolledPipe::new(driver, server)?;
    pending.set_pipe_mode(windows_sys::Win32::System::Pipes::PIPE_NOWAIT)?;
    Ok((client, server, pending))
}

pub fn file_serial(path: &Path, append: bool) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let file = fs_err::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .with_context(|| format!("failed to open serial output file '{}'", path.display()))?;
    Ok(
        serial_socket::file::FileSerialConfig::new(
            file.into(),
            path.to_string_lossy().into_owned(),
        )
        .into_resource(),
    )
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

/// Connect to an existing named pipe or Unix domain socket as a client.
///
/// Unlike [`bind_serial`], which creates a new server, this function connects
/// to a pipe or socket that already exists.
pub fn connect_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    Ok(OpenSocketSerialConfig::from(unix_socket::UnixStream::connect(path)?).into_resource())
}

pub fn bind_tcp_serial(addr: &SocketAddr) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("failed to bind tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}
