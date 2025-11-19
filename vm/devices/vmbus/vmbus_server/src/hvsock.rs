// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements an hvsocket-to-Unix socket relay.
//!
//! This supports the [hybrid vsock connection model][1] established by
//! Firecracker, extended to support Hyper-V sockets as well.
//!
//! [1]: <https://github.com/firecracker-microvm/firecracker/blob/7b2e87dc65fc45162303e5708b83c379cf1b0426/docs/vsock.md>

use super::Guid;
use crate::HvsockRelayChannelHalf;
use crate::ring::RingMem;
use anyhow::Context;
use fs_err::PathExt;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use mesh::CancelContext;
use pal_async::driver::SpawnDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use unicycle::FuturesUnordered;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use vmbus_async::pipe::BytePipe;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::offer::Offer;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;

pub struct HvsockRelay {
    inner: Arc<RelayInner>,
    host_send: mesh::Sender<RelayRequest>,
    _relay_task: Task<()>,
    _listener_task: Option<Task<()>>,
}

enum RelayRequest {
    AddTask(Task<()>),
}

struct RelayInner {
    vmbus: Arc<dyn ParentBus>,
    driver: Box<dyn SpawnDriver>,
}

impl HvsockRelay {
    /// Creates and starts a relay thread, waiting for hvsocket connect requests
    /// on `recv`.
    pub fn new(
        driver: impl SpawnDriver,
        vmbus: Arc<dyn ParentBus>,
        guest: HvsockRelayChannelHalf,
        hybrid_vsock_path: Option<PathBuf>,
        hybrid_vsock_listener: Option<UnixListener>,
    ) -> anyhow::Result<Self> {
        let inner = Arc::new(RelayInner {
            vmbus,
            driver: Box::new(driver),
        });

        let worker = HvsockRelayWorker {
            guest_send: guest.response_send,
            inner: inner.clone(),
            tasks: Default::default(),
            hybrid_vsock_path,
        };

        let (host_send, host_recv) = mesh::channel();

        let _listener_task = if let Some(listener) = hybrid_vsock_listener {
            let listener = PolledSocket::new(inner.driver.as_ref(), listener)?;
            Some(
                inner.driver.spawn(
                    "hvsock-listener",
                    ListenerWorker {
                        inner: inner.clone(),
                        host_send: host_send.clone(),
                    }
                    .run(listener),
                ),
            )
        } else {
            None
        };

        let task = inner
            .driver
            .spawn("hvsock relay", worker.run(guest.request_receive, host_recv));

        Ok(Self {
            host_send,
            inner,
            _relay_task: task,
            _listener_task,
        })
    }

    /// Connects to an hvsocket in the guest and returns a Unix socket that is
    /// relayed to the hvsocket.
    ///
    /// Blocks until complete or cancelled.
    pub fn connect(
        &self,
        ctx: &mut CancelContext,
        service_id: Guid,
    ) -> impl Future<Output = anyhow::Result<UnixStream>> + Send + use<> {
        let inner = self.inner.clone();
        let host_send = self.host_send.clone();
        let (send, recv) = mesh::oneshot();

        // Ensure the task gets dropped if the future is dropped.
        let (mut ctx, cancel) = ctx.with_cancel();

        // Spawn a task to initiate the connect to avoid keeping a reference on `RelayInner`.
        let task = self.inner.driver.spawn("hvsock-connect", async move {
            let r = async {
                let (stream, task) = ctx
                    .until_cancelled(inner.connect_to_guest(service_id))
                    .await??;
                host_send.send(RelayRequest::AddTask(task));
                Ok(stream)
            }
            .await;

            send.send(r);
        });
        self.host_send.send(RelayRequest::AddTask(task));
        async move {
            let _cancel = cancel;
            recv.await?
        }
    }
}

struct ListenerWorker {
    inner: Arc<RelayInner>,
    host_send: mesh::Sender<RelayRequest>,
}

impl ListenerWorker {
    async fn run(self, mut listener: PolledSocket<UnixListener>) {
        loop {
            let connection = match listener.accept().await {
                Ok((connection, _address)) => connection,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to accept hybrid vsock connection, shutting down listener"
                    );
                    break;
                }
            };
            match self.spawn_relay(connection).await {
                Ok(task) => {
                    self.host_send.send(RelayRequest::AddTask(task));
                }
                Err(err) => {
                    tracing::warn!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "relayed connection failed"
                    );
                }
            }
        }
    }

    async fn spawn_relay(&self, connection: UnixStream) -> anyhow::Result<Task<()>> {
        let mut socket = PolledSocket::new(self.inner.driver.as_ref(), connection)?;
        let (service_id, format) = read_hybrid_vsock_connect(&mut socket).await?;

        let instance_id = Guid::new_random();
        let mut offer = Offer::new(
            self.inner.driver.as_ref(),
            self.inner.vmbus.as_ref(),
            OfferParams {
                interface_name: "hvsocket_connect".into(),
                interface_id: service_id,
                instance_id,
                channel_type: ChannelType::HvSocket {
                    is_connect: true,
                    is_for_container: false,
                    silo_id: Guid::ZERO,
                },
                ..Default::default()
            },
        )
        .await
        .context("failed to offer channel")?;

        let channel = CancelContext::new()
            .with_timeout(Duration::from_secs(2))
            .until_cancelled(offer.wait_for_open(self.inner.driver.as_ref()))
            .await?
            .context("failed to accept channel")?
            .accept()
            .channel;

        let pipe = BytePipe::new(channel).context("failed to create vmbus pipe")?;

        tracing::debug!(%service_id, endpoint_id = %instance_id, "connected host to guest");

        let task = self
            .inner
            .driver
            .spawn("hvsock connection relay", async move {
                // Keep the offer alive until the relay completes.
                let _offer = offer;

                // Notify the client that connection was successful.
                let s = match format {
                    ServiceIdFormat::Vsock => format!("OK {}\n", instance_id.data1),
                    ServiceIdFormat::HyperV => format!("OK {}\n", instance_id),
                };
                if let Err(err) = socket.write_all(s.as_bytes()).await {
                    tracing::error!(
                        %service_id,
                        error = &err as &dyn std::error::Error,
                        "failed to write OK response"
                    );
                }

                if let Err(err) = relay_connected(pipe, socket).await {
                    tracing::error!(
                        %service_id,
                        error = &err as &dyn std::error::Error,
                        "connection relay failed"
                    );
                } else {
                    tracing::debug!(%service_id, "connection relay finished");
                }
            });

        Ok(task)
    }
}

#[derive(Debug)]
enum ServiceIdFormat {
    Vsock,
    HyperV,
}

async fn read_hybrid_vsock_connect(
    socket: &mut PolledSocket<UnixStream>,
) -> anyhow::Result<(Guid, ServiceIdFormat)> {
    let mut buf = [0; "CONNECT 00000000-facb-11e6-bd58-64006a7986d3\n".len()];
    let mut i = 0;
    while i == 0 || buf[i - 1] != b'\n' {
        if i == buf.len() {
            anyhow::bail!("connect request did not fit");
        }
        let n = socket
            .read(&mut buf[i..])
            .await
            .context("failed to read connect request")?;
        if n == 0 {
            anyhow::bail!("no connect request");
        }
        i += n;
    }

    let rest = buf[..i - 1]
        .strip_prefix(b"CONNECT ")
        .context("invalid connect request")?;

    let rest = std::str::from_utf8(rest).context("invalid connect request")?;
    let (service_id, format) = if let Ok(port) = rest.parse::<u32>() {
        (
            Guid {
                data1: port,
                ..VSOCK_TEMPLATE
            },
            ServiceIdFormat::Vsock,
        )
    } else if let Ok(service_id) = rest.parse::<Guid>() {
        (service_id, ServiceIdFormat::HyperV)
    } else {
        anyhow::bail!("invalid port or service ID: {}", rest);
    };

    tracing::debug!(%service_id, ?format, "got hybrid connect request");
    Ok((service_id, format))
}

struct PendingConnection {
    send: mesh::Sender<HvsockConnectResult>,
    request: HvsockConnectRequest,
}

impl PendingConnection {
    fn done(self, success: bool) {
        self.send
            .send(HvsockConnectResult::from_request(&self.request, success));
        std::mem::forget(self);
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        self.send
            .send(HvsockConnectResult::from_request(&self.request, false));
    }
}

// This GUID is an embedding of the AF_VSOCK port into an
// AF_HYPERV service ID.
static VSOCK_TEMPLATE: Guid = guid::guid!("00000000-facb-11e6-bd58-64006a7986d3");

fn vsock_port(service_id: &Guid) -> Option<u32> {
    let stripped_id = Guid {
        data1: 0,
        ..*service_id
    };
    (VSOCK_TEMPLATE == stripped_id).then_some(service_id.data1)
}

struct HvsockRelayWorker {
    guest_send: mesh::Sender<HvsockConnectResult>,
    tasks: FuturesUnordered<Task<()>>,
    inner: Arc<RelayInner>,
    hybrid_vsock_path: Option<PathBuf>,
}

impl HvsockRelayWorker {
    async fn run(
        mut self,
        guest_recv: mesh::Receiver<HvsockConnectRequest>,
        host_recv: mesh::Receiver<RelayRequest>,
    ) {
        enum Event {
            Guest(HvsockConnectRequest),
            Host(RelayRequest),
            TaskDone(()),
        }

        let mut recv = (guest_recv.map(Event::Guest), host_recv.map(Event::Host)).merge();

        while let Some(event) = (&mut recv, (&mut self.tasks).map(Event::TaskDone))
            .merge()
            .next()
            .await
        {
            match event {
                Event::Guest(request) => {
                    self.handle_connect_from_guest(request);
                }
                Event::Host(request) => match request {
                    RelayRequest::AddTask(task) => {
                        self.tasks.push(task);
                    }
                },
                Event::TaskDone(()) => {}
            }
        }
    }

    fn handle_connect_from_guest(&mut self, request: HvsockConnectRequest) {
        if request.silo_id != Guid::ZERO {
            tracelimit::warn_ratelimited!(?request, "Non-zero silo ID is currently ignored.")
        }

        // Wrap the connect request so that we are assured to send a response.
        let pending = PendingConnection {
            send: self.guest_send.clone(),
            request,
        };
        let (path, is_specific_path) = {
            if let Some(hybrid_vsock_path) = &self.hybrid_vsock_path {
                (hybrid_vsock_path.to_owned(), false)
            } else {
                tracing::debug!(request = ?&request, "ignoring hvsock connect request");
                return;
            }
        };

        let task = self.inner.driver.spawn(
            format!(
                "hvsock accept {}:{}",
                request.service_id, request.endpoint_id
            ),
            {
                let inner = self.inner.clone();
                async move {
                    match inner
                        .relay_guest_connect_to_host(pending, path.as_ref(), is_specific_path)
                        .await
                    {
                        Ok(()) => {
                            tracing::debug!(request = ?&request, "relay done");
                        }
                        Err(err) => {
                            tracelimit::error_ratelimited!(
                                request = ?&request,
                                err = err.as_ref() as &dyn std::error::Error,
                                "relay error"
                            );
                        }
                    }
                }
            },
        );
        self.tasks.push(task);
    }
}

impl RelayInner {
    async fn relay_guest_connect_to_host(
        &self,
        pending: PendingConnection,
        base_path: &Path,
        is_specific_path: bool,
    ) -> anyhow::Result<()> {
        let request = &pending.request;

        // Find the appropriate path to connect to. Don't connect until the
        // channel with the guest has been established, since that's a
        // failure-prone operation and we don't want the host to see a broken
        // connection.
        let path = self.host_uds_path(request, base_path, is_specific_path)?;

        let mut offer = Offer::new(
            self.driver.as_ref(),
            self.vmbus.as_ref(),
            OfferParams {
                interface_name: "hvsocket".to_owned(),
                instance_id: request.endpoint_id,
                interface_id: request.service_id,
                channel_type: ChannelType::HvSocket {
                    is_connect: false,
                    is_for_container: false,
                    silo_id: Guid::ZERO,
                },
                ..Default::default()
            },
        )
        .await
        .context("failed to offer channel")?;

        tracing::debug!(?request, "offered hvsocket channel to guest");
        let service_id = request.service_id;

        // Now that the channel is offered, report that the connection operation is
        // done.
        pending.done(true);

        // Give the guest a few seconds to open the channel.
        let channel = CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(offer.wait_for_open(self.driver.as_ref()))
            .await
            .context("guest did not open hvsocket channel")??;

        tracing::debug!(%service_id, "guest opened hvsocket channel");

        // Connect to the host Unix socket.
        let socket = PolledSocket::connect_unix(self.driver.as_ref(), &path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to registered listener {} for {}",
                    path.display(),
                    service_id
                )
            })?;

        tracing::debug!(%service_id, path = %path.display(), "connected to host uds socket");

        // Accept the channel now that the host connection is established.
        let channel = channel.accept().channel;

        let channel = BytePipe::new(channel)?;
        if let Err(err) = relay_connected(channel, socket).await {
            tracelimit::error_ratelimited!(
                %service_id,
                error = &err as &dyn std::error::Error,
                "guest to host connection relay failed"
            );
        } else {
            tracing::debug!(%service_id, "guest to host connection relay finished");
        }

        // N.B. offer needs to stay alive until here to avoid revoking the channel
        // before the relay is done.
        drop(offer);
        Ok(())
    }

    fn host_uds_path(
        &self,
        request: &HvsockConnectRequest,
        base_path: &Path,
        is_specific_path: bool,
    ) -> anyhow::Result<PathBuf> {
        let mut path = base_path.as_os_str().to_owned();
        if !is_specific_path {
            if let Some(port) = vsock_port(&request.service_id) {
                // This is a vsock connection, so try connecting after appending the
                // port to the path.
                path.push(format!("_{port}"));
                if Path::new(&path).fs_err_try_exists()? {
                    return Ok(path.into());
                }
                path.clear();
                path.push(base_path);
            }
            path.push(format!("_{}", request.service_id));
        }
        if !Path::new(&path).fs_err_try_exists()? {
            anyhow::bail!(
                "no hybrid vsock listener based at {} for {}",
                base_path.display(),
                request.service_id
            );
        }
        Ok(path.into())
    }

    async fn connect_to_guest(&self, service_id: Guid) -> anyhow::Result<(UnixStream, Task<()>)> {
        let instance_id = Guid::new_random();
        let mut offer = Offer::new(
            &self.driver,
            self.vmbus.as_ref(),
            OfferParams {
                interface_name: "hvsocket_connect".into(),
                interface_id: service_id,
                instance_id,
                channel_type: ChannelType::HvSocket {
                    is_connect: true,
                    is_for_container: false,
                    silo_id: Guid::ZERO,
                },
                ..Default::default()
            },
        )
        .await
        .context("failed to offer channel")?;

        let channel = offer
            .wait_for_open(self.driver.as_ref())
            .await
            .context("failed to accept channel")?
            .accept()
            .channel;
        let pipe = BytePipe::new(channel).context("failed to create vmbus pipe")?;

        tracing::debug!(%service_id, endpoint_id = %instance_id, "connected host to guest");

        let (left, right) = UnixStream::pair().context("failed to create socket pair")?;
        let right = PolledSocket::new(self.driver.as_ref(), right)
            .context("failed to create polled socket")?;

        let task = self.driver.spawn(
            format!("hvsock {}:{}", service_id, instance_id),
            async move {
                // Keep the offer alive until the relay completes.
                let _offer = offer;
                if let Err(err) = relay_connected(pipe, right).await {
                    tracing::error!(
                        %service_id,
                        error = &err as &dyn std::error::Error,
                        "connection relay failed"
                    );
                }
            },
        );

        Ok((left, task))
    }
}

async fn relay_connected<T: RingMem + Unpin>(
    channel: BytePipe<T>,
    socket: PolledSocket<UnixStream>,
) -> std::io::Result<()> {
    let (channel_read, mut channel_write) = channel.split();
    let (socket_read, mut socket_write) = socket.split();

    let channel_to_socket = async {
        futures::io::copy(channel_read, &mut socket_write).await?;
        socket_write.close().await
    };

    let socket_to_channel = async {
        futures::io::copy(socket_read, &mut channel_write).await?;
        channel_write.close().await
    };

    match futures::future::try_join(channel_to_socket, socket_to_channel).await {
        Ok(((), ())) => {}
        Err(err) if err.kind() == ErrorKind::ConnectionReset => {}
        Err(err) => return Err(err),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::relay_connected;
    use crate::ring::FlatRingMem;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::driver::Driver;
    use pal_async::socket::PolledSocket;
    use pal_async::task::Spawn;
    use pal_async::task::Task;
    use unix_socket::UnixStream;
    use vmbus_async::pipe::BytePipe;
    use vmbus_async::pipe::connected_byte_pipes;

    fn setup_relay<T: Driver + Spawn>(
        driver: &T,
    ) -> (
        BytePipe<FlatRingMem>,
        PolledSocket<UnixStream>,
        Task<std::io::Result<()>>,
    ) {
        let (hc, c) = connected_byte_pipes(4096);
        let (s, s2) = UnixStream::pair().unwrap();
        let s = PolledSocket::new(driver, s).unwrap();
        let s2 = PolledSocket::new(driver, s2).unwrap();
        let task = driver.spawn("test", async move { relay_connected(hc, s2).await });

        (c, s, task)
    }

    #[async_test]
    async fn test_relay(driver: DefaultDriver) {
        let (mut c, mut s, task) = setup_relay(&driver);

        let d = b"abcd";
        let mut v = [0; 4];

        // c to s
        c.write_all(d).await.unwrap();
        s.read_exact(&mut v).await.unwrap();
        assert_eq!(&v, d);

        // s to c
        s.write_all(d).await.unwrap();
        c.read_exact(&mut v).await.unwrap();
        assert_eq!(&v, d);

        // s to c
        s.write_all(d).await.unwrap();
        s.close().await.unwrap();
        c.read_exact(&mut v).await.unwrap();
        assert_eq!(&v, d);

        // c to s
        c.write_all(d).await.unwrap();
        s.read_exact(&mut v).await.unwrap();
        assert_eq!(&v, d);

        c.close().await.unwrap();
        task.await.unwrap();
    }

    #[cfg(unix)] // Windows does not deliver POLLHUP on Unix socket close.
    #[async_test]
    async fn test_relay_host_close(driver: DefaultDriver) {
        let (mut c, _, task) = setup_relay(&driver);

        let mut b = [0];
        assert_eq!(c.read(&mut b).await.unwrap(), 0);
        drop(c);
        task.await.unwrap();
    }

    #[async_test]
    async fn test_relay_guest_close(driver: DefaultDriver) {
        let (_, mut s, task) = setup_relay(&driver);

        let mut b = [0];
        assert_eq!(s.read(&mut b).await.unwrap(), 0);
        drop(s);
        task.await.unwrap();
    }

    #[async_test]
    async fn test_relay_forward_socket_shutdown(driver: DefaultDriver) {
        let (mut c, mut s, task) = setup_relay(&driver);
        s.close().await.unwrap();
        let mut v = [0; 1];
        assert_eq!(c.read(&mut v).await.unwrap(), 0);
        drop(c);
        task.await.unwrap();
    }

    #[async_test]
    async fn test_relay_forward_channel_shutdown(driver: DefaultDriver) {
        let (mut c, mut s, task) = setup_relay(&driver);

        c.close().await.unwrap();
        let mut v = [0; 1];
        assert_eq!(s.read(&mut v).await.unwrap(), 0);
        drop(s);
        task.await.unwrap();
    }
}
