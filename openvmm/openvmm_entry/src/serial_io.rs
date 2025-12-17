// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cleanup_socket;
use anyhow::Context;
use futures::StreamExt;
use futures::stream;
use futures_concurrency::prelude::*;
use io::ErrorKind;
use io::Read;
use openvmm_defs::config::SerialPipes;
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_async::pipe::PolledPipe;
use pal_async::task::Task;
use serial_socket::net::OpenSocketSerialConfig;
use std::fs::File;
use std::io;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::thread;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

pub struct SerialIo {
    pub input: Option<File>,
    pub output: Option<File>,
    pub config: SerialPipes,
}

impl SerialIo {
    pub fn new() -> io::Result<Self> {
        let (op, oc) = PolledPipe::file_pair()?;
        let (ic, ip) = PolledPipe::file_pair()?;
        Ok(Self {
            input: Some(ip),
            output: Some(op),
            config: SerialPipes {
                input: Some(ic),
                output: Some(oc),
            },
        })
    }

    pub fn spawn_copy_out(&mut self, name: &str, mut f: impl Write + Send + 'static) {
        if let Some(mut output) = self.output.take() {
            thread::Builder::new()
                .name(format!("{} copy out", name))
                .spawn(move || {
                    loop {
                        let mut buf = [0; 256];
                        let n = output.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        f.write_all(&buf[..n]).expect("BUGBUG");
                        f.flush().expect("BUGBUG");
                    }
                })
                .unwrap();
        }
    }

    pub fn spawn_copy_listener(
        &mut self,
        driver: impl SpawnDriver + Clone,
        name: &str,
        path: &Path,
    ) -> anyhow::Result<Task<()>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            // Delete the specified path if it's a socket so that we can rebind
            // to the same path.
            if let Ok(meta) = path.metadata() {
                if meta.file_type().is_socket() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        let mut listener;
        #[cfg(windows)]
        {
            listener = pal_async::windows::pipe::NamedPipeServer::create(path)?;
        }

        #[cfg(unix)]
        {
            listener = pal_async::socket::PolledSocket::new(&driver, UnixListener::bind(path)?)
                .context("failed to create polled socket for listener")?;
        }

        let input = self.input.take().unwrap();
        let output = self.output.take().unwrap();
        let path = path.to_owned();
        let mut output =
            PolledPipe::new(&driver, output).context("failed to create polled pipe")?;
        let mut input = PolledPipe::new(&driver, input).context("failed to create polled pipe")?;

        let task = driver.spawn(format!("{} copy listener", name), {
            let driver = driver.clone();
            async move {
                loop {
                    if let Err(err) =
                        relay_pipes(&driver, &mut listener, &mut output, &mut input).await
                    {
                        tracing::error!(
                            path = %path.display(),
                            error = err.as_ref() as &dyn std::error::Error,
                            "pipe relay failed"
                        );
                    } else {
                        tracing::debug!(path = %path.display(), "pipe relay done");
                    }
                }
            }
        });
        Ok(task)
    }
}

// On Windows, serial listeners are backed by named pipes.
#[cfg(windows)]
type SerialListener = pal_async::windows::pipe::NamedPipeServer;

// On Unix, serial listeners are backed by Unix sockets.
#[cfg(unix)]
type SerialListener = pal_async::socket::PolledSocket<UnixListener>;

async fn relay_pipes(
    driver: &impl Driver,
    left_listener: &mut SerialListener,
    right_read: &mut PolledPipe,
    right_write: &mut PolledPipe,
) -> anyhow::Result<()> {
    loop {
        let left_connection;
        let (left_read, mut left_write);

        #[cfg(windows)]
        {
            let pipe = left_listener.accept(driver)?.await?;
            left_connection = PolledPipe::new(driver, pipe)?;
            (left_read, left_write) = futures::AsyncReadExt::split(left_connection);
        }

        #[cfg(unix)]
        {
            let (conn, _) = left_listener
                .accept()
                .await
                .context("failed to accept socket")?;
            left_connection = pal_async::socket::PolledSocket::new(driver, conn)
                .context("failed to create polled socket for connection")?;

            (left_read, left_write) = left_connection.split();
        }

        enum Event {
            LeftToRight(io::Result<u64>),
            RightToLeft(io::Result<u64>),
        }

        let a = stream::once(futures::io::copy(&mut *right_read, &mut left_write))
            .map(Event::LeftToRight);
        let b = stream::once(futures::io::copy(left_read, right_write)).map(Event::RightToLeft);
        let mut s = (a, b).merge();

        while let Some(event) = s.next().await {
            match event {
                Event::LeftToRight(r) => {
                    let _ = r.context("failed to copy to serial port")?;
                    // The client disconnected, so break out of this loop to
                    // wait for another connection.
                    break;
                }
                Event::RightToLeft(r) => {
                    match r {
                        Ok(_) => {
                            // The VM disconnected, so it is not waiting for any
                            // more data. Break out.
                            return Ok(());
                        }
                        Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                            // The client disconnected. Continue in this loop to
                            // drain anything in the client's buffer before
                            // accepting a new connection.
                        }
                        Err(err) => {
                            return Err(err).context("failed to copy from serial port");
                        }
                    }
                }
            }
        }
    }
}

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
                winapi::um::winnt::GENERIC_READ | winapi::um::winnt::GENERIC_WRITE,
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
