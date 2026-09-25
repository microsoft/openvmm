// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial endpoint helpers beyond the upstream ones: connecting to an existing
//! endpoint as a client within a bounded time.

use anyhow::Context;
use serial_socket::net::OpenSocketSerialConfig;
use std::io;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

pub(crate) fn connect_serial_with_timeout(
    path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Resource<SerialBackendHandle>> {
    let path = path.to_owned();
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("serial-connect".to_owned())
        .spawn(move || {
            let _ = send.send(super::connect_serial(&path));
        })?;
    match recv.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("serial endpoint did not connect within {timeout:?}"),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
            "serial connection worker terminated without a result",
        )),
    }
}

pub(crate) fn connect_tcp_serial(
    addr: &SocketAddr,
    timeout: std::time::Duration,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let stream = TcpStream::connect_timeout(addr, timeout)
        .with_context(|| format!("failed to connect to tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(stream).into_resource())
}
