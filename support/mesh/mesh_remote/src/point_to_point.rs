// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Point-to-point mesh implementation.

use futures::future::try_join;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::StreamExt;
use futures::TryFutureExt;
use futures_concurrency::future::Race;
use mesh_channel::cancel::Cancel;
use mesh_channel::cancel::CancelContext;
use mesh_channel::cancel::CancelReason;
use mesh_node::common::Address;
use mesh_node::common::NodeId;
use mesh_node::common::PortId;
use mesh_node::common::Uuid;
use mesh_node::local_node::Connect;
use mesh_node::local_node::LocalNode;
use mesh_node::local_node::OutgoingEvent;
use mesh_node::local_node::Port;
use mesh_node::local_node::SendEvent;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::io;
use std::pin::pin;
use thiserror::Error;
use tracing::Instrument;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A mesh that consists of exactly two nodes, communicating over an arbitrary
/// bidirectional byte stream.
///
/// This byte stream could be a stream socket, a Windows named pipe, or a serial
/// port, for example.
///
/// There is no support for OS resources (handles or file descriptors) in this
/// mesh implementation. Any attempt to send OS resources will fail the
/// underlying channel.
#[must_use]
pub struct PointToPointMesh {
    task: Task<()>,
    cancel: Cancel,
}

impl PointToPointMesh {
    /// Makes a new mesh over the connection `conn`, with initial port `port`.
    ///
    /// ```rust
    /// # use mesh_remote::PointToPointMesh;
    /// # use mesh_channel::channel;
    /// # use unix_socket::UnixStream;
    /// # use pal_async::socket::PolledSocket;
    /// # pal_async::DefaultPool::run_with(|driver| async move {
    /// let (left, right) = UnixStream::pair().unwrap();
    /// let (a, ax) = channel::<u32>();
    /// let (bx, mut b) = channel::<u32>();
    /// let left = PointToPointMesh::new(&driver, PolledSocket::new(&driver, left).unwrap(), ax.into());
    /// let right = PointToPointMesh::new(&driver, PolledSocket::new(&driver, right).unwrap(), bx.into());
    /// a.send(5);
    /// assert_eq!(b.recv().await.unwrap(), 5);
    /// # })
    /// ```
    pub fn new(
        spawn: impl Spawn,
        conn: impl 'static + AsyncRead + AsyncWrite + Send + Unpin,
        port: Port,
    ) -> Self {
        let local_address = Address {
            node: NodeId::new(),
            port: PortId::new(),
        };
        let (mut ctx, cancel) = CancelContext::new().with_cancel();
        let task = spawn.spawn(
            format!("mesh-point-to-point-{:?}", local_address.node),
            async move {
                if let Err(err) = handle_comms(&mut ctx, Box::new(conn), local_address, port).await
                {
                    tracing::error!(error = &err as &dyn std::error::Error, "io failure");
                }
            }
            .instrument(tracing::info_span!("mesh-point-to-point", node = ?local_address.node)),
        );

        Self { task, cancel }
    }

    /// Shuts down the mesh. Any pending messages are dropped.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        self.task.await;
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

#[derive(Debug, Error)]
enum TaskError {
    #[error("cancelled")]
    Cancelled(#[from] CancelReason),
    #[error("failed to change addresses")]
    Exchange(#[source] io::Error),
    #[error("failed to send data")]
    Send(#[source] io::Error),
    #[error("failed to receive data")]
    Recv(#[source] io::Error),
}

async fn handle_comms(
    ctx: &mut CancelContext,
    conn: Box<dyn AsyncReadWrite>,
    local_address: Address,
    port: Port,
) -> Result<(), TaskError> {
    let (mut read, mut write) = conn.split();
    let node = LocalNode::with_id(local_address.node, Box::new(NullConnector));

    tracing::debug!("exchanging addresses");
    let remote_address = ctx
        .until_cancelled(exchange_addresses(local_address, &mut write, &mut read))
        .await?
        .map_err(TaskError::Exchange)?;

    tracing::debug!(?local_address, ?remote_address, "connected to remote node");

    let remote = node.add_remote(remote_address.node);
    let (send_event, recv_event) = mesh_channel::channel();
    remote.connect(PointToPointConnection(send_event));
    let init_port = node.add_port(local_address.port, remote_address);
    init_port.bridge(port);

    let recv_loop = recv_loop(&remote_address.node, read, &node).map_err(TaskError::Recv);
    let send_loop = send_loop(recv_event, write).map_err(TaskError::Send);

    // Run until either send or receive finishes. If sending is done, then the
    // remote node has been disconnected from `LocalNode`, so no more events
    // need to be received. If receiving is done, then the remote node has
    // disconnected its pipe, so it will not be accepting any more events.
    let mut fut = pin!((recv_loop, send_loop).race());

    let r = match ctx.until_cancelled(fut.as_mut()).await {
        Ok(r) => r,
        Err(_) => {
            let shutdown = async {
                node.wait_for_ports(false).await;
                node.fail_all_nodes();
                Ok(())
            };
            try_join(shutdown, fut).await.map(|((), ())| ())
        }
    };
    match r {
        Ok(()) => remote.disconnect(),
        Err(err) => remote.fail(err),
    }
    Ok(())
}

async fn exchange_addresses(
    local_address: Address,
    write: &mut (impl AsyncWrite + Unpin),
    read: &mut (impl AsyncRead + Unpin),
) -> io::Result<Address> {
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct Message {
        magic: [u8; 4],
        node: [u8; 16],
        port: [u8; 16],
    }

    const MAGIC: [u8; 4] = *b"mesh";
    let local_msg = Message {
        magic: MAGIC,
        node: (local_address.node.0).0,
        port: (local_address.port.0).0,
    };

    let mut remote_msg = Message::new_zeroed();
    try_join(
        write.write_all(local_msg.as_bytes()),
        read.read_exact(remote_msg.as_mut_bytes()),
    )
    .await?;

    if remote_msg.magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid address header",
        ));
    }

    Ok(Address::new(
        NodeId(Uuid(remote_msg.node)),
        PortId(Uuid(remote_msg.port)),
    ))
}

async fn recv_loop(
    remote_id: &NodeId,
    read: impl AsyncRead + Unpin,
    node: &LocalNode,
) -> io::Result<()> {
    let mut read = BufReader::new(read);
    loop {
        let mut b = [0; 8];
        if read.fill_buf().await?.is_empty() {
            break;
        }
        read.read_exact(&mut b).await?;
        let len = u64::from_le_bytes(b) as usize;
        let buf = read.buffer();
        if buf.len() >= len {
            // Parse the event directly from the buffer.
            node.event(remote_id, buf, &mut Vec::new());
            read.consume_unpin(len);
        } else {
            // Read the whole event into a new buffer.
            let mut b = vec![0; len];
            read.read_exact(&mut b).await?;
            node.event(remote_id, &b, &mut Vec::new());
        }
    }
    tracing::debug!("recv loop done");
    Ok(())
}

async fn send_loop(
    mut recv_event: mesh_channel::Receiver<Vec<u8>>,
    mut write: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    while let Some(event) = recv_event.next().await {
        write.write_all(&(event.len() as u64).to_le_bytes()).await?;
        write.write_all(&event).await?;
    }
    tracing::debug!("send loop done");
    Ok(())
}

#[derive(Debug)]
struct PointToPointConnection(mesh_channel::Sender<Vec<u8>>);

impl SendEvent for PointToPointConnection {
    fn event(&self, event: OutgoingEvent<'_>) {
        let len = event.len();
        let mut v = Vec::with_capacity(len);
        let mut resources = Vec::new();
        event.write_to(&mut v, &mut resources);
        if !resources.is_empty() {
            // Still send the message so that the receiving side gets an error
            // when decoding. Otherwise, the only other option at this point is
            // to fail the whole connection, which is probably not what you
            // want.
            tracing::warn!("cannot send OS resources across a point-to-point connection");
        }
        self.0.send(v);
    }
}

#[derive(Debug)]
struct NullConnector;

impl Connect for NullConnector {
    fn connect(&self, _node_id: NodeId, handle: mesh_node::local_node::RemoteNodeHandle) {
        handle.fail(NoMesh);
    }
}

#[derive(Debug, Error)]
#[error("no extra connections allowed in point-to-point mesh")]
struct NoMesh;

#[cfg(test)]
mod tests {
    use super::PointToPointMesh;
    use mesh_channel::channel;
    use pal_async::async_test;
    use pal_async::socket::PolledSocket;
    use pal_async::DefaultDriver;
    use test_with_tracing::test;
    use unix_socket::UnixStream;

    #[async_test]
    async fn test_point_to_point(driver: DefaultDriver) {
        let (left, right) = UnixStream::pair().unwrap();
        let left = PolledSocket::new(&driver, left).unwrap();
        let right = PolledSocket::new(&driver, right).unwrap();
        let (a, ax) = channel::<u32>();
        let (bx, mut b) = channel::<u32>();
        let left = PointToPointMesh::new(&driver, left, ax.into());
        let right = PointToPointMesh::new(&driver, right, bx.into());
        a.send(5);
        assert_eq!(b.recv().await.unwrap(), 5);
        left.shutdown().await;
        right.shutdown().await;
    }
}
