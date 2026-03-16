// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use futures::AsyncWrite;
use pal_async::socket::PolledSocket;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use vmcore::vm_task::VmTaskDriver;

pub struct UnixSocketRelay {
    driver: VmTaskDriver,
    base_path: PathBuf,
}

impl UnixSocketRelay {
    pub fn new(driver: VmTaskDriver, base_path: PathBuf) -> Self {
        Self { driver, base_path }
    }

    pub fn connect(&self, port: u32) -> anyhow::Result<PolledSocket<UnixStream>> {
        let socket_path = format!("{}_{}", self.base_path.to_str().unwrap(), port);
        tracing::info!(
            "Connecting to Unix socket for vsock relay: {:?}",
            socket_path
        );
        let stream = UnixStream::connect(socket_path)
            .with_context(|| "Failed to connect to Unix socket for vsock relay")?;
        let socket = PolledSocket::new(&self.driver, stream)
            .with_context(|| "Failed to create polled socket for vsock relay")?;

        Ok(socket)
    }
}
