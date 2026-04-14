// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker for running a VNC server.

#![forbid(unsafe_code)]

use anyhow::Context;
use anyhow::anyhow;
use futures::FutureExt;
use futures::StreamExt;
use input_core::InputData;
use input_core::KeyboardData;
use input_core::MouseData;
use mesh::message::MeshField;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::local::LocalDriver;
use pal_async::local::block_with_io;
use pal_async::socket::Listener;
use pal_async::socket::PolledSocket;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use std::future::Future;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing_helpers::AnyhowValueExt;
use vnc_worker_defs::VncParameters;

/// A worker for running a VNC server.
pub struct VncWorker<T: Listener> {
    listener: T,
    view: ViewWrapper,
    input_send: mesh::Sender<InputData>,
    dirty_recv: Option<mesh::Receiver<Vec<video_core::DirtyRect>>>,
    max_clients: usize,
    evict_oldest: bool,
}

impl Worker for VncWorker<TcpListener> {
    type Parameters = VncParameters<TcpListener>;
    type State = VncParameters<TcpListener>;
    const ID: WorkerId<Self::Parameters> = vnc_worker_defs::VNC_WORKER_TCP;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl Worker for VncWorker<vmsocket::VmListener> {
    type Parameters = VncParameters<vmsocket::VmListener>;
    type State = VncParameters<vmsocket::VmListener>;
    const ID: WorkerId<Self::Parameters> = vnc_worker_defs::VNC_WORKER_VMSOCKET;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

impl<T: 'static + Listener + MeshField + Send> VncWorker<T> {
    fn new_inner(params: VncParameters<T>) -> anyhow::Result<Self> {
        Ok(Self {
            listener: params.listener,
            view: ViewWrapper(
                params
                    .framebuffer
                    .view()
                    .context("failed to map framebuffer")?,
            ),
            input_send: params.input_send,
            dirty_recv: params.dirty_recv,
            max_clients: params.max_clients,
            evict_oldest: params.evict_oldest,
        })
    }

    fn run_inner(
        self,
        mut rpc_recv: mesh::Receiver<WorkerRpc<VncParameters<T>>>,
    ) -> anyhow::Result<()> {
        block_with_io(async |driver| {
            tracing::info!(
                address = ?self.listener.local_addr().unwrap(),
                "VNC server listening",
            );

            let listener = PolledSocket::new(&driver, self.listener)?;
            let mut server = MultiClientServer {
                listener,
                view: Arc::new(Mutex::new(self.view)),
                input_send: self.input_send,
                dirty_recv: self.dirty_recv,
                dirty_senders: Vec::new(),
                clients: unicycle::FuturesUnordered::new(),
                abort_senders: Vec::new(),
                next_client_id: 0,
                max_clients: self.max_clients,
                evict_oldest: self.evict_oldest,
            };

            let rpc = loop {
                let r = futures::select! { // merge semantics
                    r = rpc_recv.recv().fuse() => r,
                    r = server.process(&driver).fuse() => break r.map(|_| None)?,
                };
                match r {
                    Ok(message) => match message {
                        WorkerRpc::Stop => break None,
                        WorkerRpc::Inspect(deferred) => deferred.inspect(&server),
                        WorkerRpc::Restart(response) => break Some(response),
                    },
                    Err(_) => break None,
                }
            };
            if let Some(rpc) = rpc {
                // Abort all active clients before recovering shared state.
                server.abort_all_clients().await;
                let view = Arc::try_unwrap(server.view)
                    .expect("all clients terminated")
                    .into_inner();
                let state = VncParameters {
                    listener: server.listener.into_inner(),
                    framebuffer: view.0.access(),
                    input_send: server.input_send,
                    dirty_recv: server.dirty_recv,
                    max_clients: server.max_clients,
                    evict_oldest: server.evict_oldest,
                };
                rpc.complete(Ok(state));
            }
            Ok(())
        })
    }
}

/// A multi-client VNC server that accepts and manages concurrent connections.
struct MultiClientServer<T: Listener> {
    listener: PolledSocket<T>,
    /// Shared framebuffer view, protected by a mutex since reads mutate
    /// internal state (channel polling in `resolution()`).
    view: Arc<Mutex<ViewWrapper>>,
    /// Cloneable input sender -- each client gets its own clone.
    input_send: mesh::Sender<InputData>,
    /// Dirty rectangles from the synthetic video device. None if no video
    /// device is configured.
    dirty_recv: Option<mesh::Receiver<Vec<video_core::DirtyRect>>>,
    /// Per-client dirty rect senders. The coordinator broadcasts device rects
    /// to all clients via these channels. Each client accumulates rects in
    /// its own pending bitmap -- no shared state, no clearing issues.
    dirty_senders: Vec<(
        u64,
        futures::channel::mpsc::Sender<Arc<Vec<video_core::DirtyRect>>>,
        Arc<AtomicBool>,
    )>,
    /// Futures for all active client connections. Each resolves to the
    /// client's id when the connection ends.
    clients: unicycle::FuturesUnordered<Pin<Box<dyn Future<Output = u64>>>>,
    /// Abort senders for each client, keyed by client id. Dropping the
    /// sender closes the oneshot channel, which the client detects as
    /// an abort signal.
    abort_senders: Vec<(u64, mesh::OneshotSender<()>)>,
    next_client_id: u64,
    /// Maximum concurrent clients. Bounds memory (~8MB per client for
    /// framebuffer buffers) and prevents VRAM mutex contention.
    max_clients: usize,
    /// When true, evict the oldest client instead of rejecting new ones.
    evict_oldest: bool,
}

impl<T: Listener> MultiClientServer<T> {
    /// Main loop: accept new clients, reap finished ones, and broadcast
    /// device dirty rects to per-client channels.
    async fn process(&mut self, driver: &LocalDriver) -> anyhow::Result<()> {
        enum Event<A> {
            Accepted(A),
            ClientDone(u64),
            DirtyRects(Vec<video_core::DirtyRect>),
        }

        let mut device_dirty_seen = false;

        loop {
            let listener = &mut self.listener;
            let clients = &mut self.clients;
            let dirty_recv = &mut self.dirty_recv;

            // Optional future for dirty rect reception (pending if no video device).
            let dirty_fut = async {
                match dirty_recv {
                    Some(recv) => recv.recv().await,
                    None => std::future::pending().await,
                }
            };

            // Optional future for client completion (pending if no clients).
            // A separate future avoids duplicating the entire select! block.
            let client_done = async {
                if clients.is_empty() {
                    std::future::pending().await
                } else {
                    clients.select_next_some().await
                }
            };

            let event = futures::select! {
                accept = listener.accept().fuse() => {
                    let (socket, addr) = accept?;
                    Event::Accepted((socket, addr))
                }
                id = client_done.fuse() => Event::ClientDone(id),
                msg = dirty_fut.fuse() => match msg {
                    Ok(rects) => Event::DirtyRects(rects),
                    Err(_) => {
                        // Upstream dirty channel closed (video device reset
                        // or teardown). Drop it so clients fall back to tile
                        // diff instead of freezing with device_dirty_seen.
                        tracing::warn!("device dirty channel closed, falling back to tile diff");
                        self.dirty_recv = None;
                        // Close all per-client dirty senders so clients
                        // detect the closure and reset device_dirty_seen.
                        self.dirty_senders.clear();
                        continue;
                    }
                }
            };

            match event {
                Event::Accepted((socket, remote_addr)) => {
                    // Use abort_senders.len() as the active client count,
                    // not clients.len(). Evicted clients are removed from
                    // abort_senders immediately but their futures may linger
                    // in self.clients until the next poll reaps them via
                    // ClientDone. This means self.clients.len() can transiently
                    // exceed max_clients during rapid connection churn (e.g.,
                    // max_clients=1 with A→B→C arriving before A is reaped).
                    // This is acceptable: the dying futures are in their abort
                    // cleanup path and resolve within one poll cycle. Only
                    // abort_senders.len() clients are actively running the VNC
                    // protocol. Awaiting the evicted client before spawning
                    // the replacement would add latency to every new connection.
                    if self.abort_senders.len() >= self.max_clients {
                        if self.evict_oldest && !self.abort_senders.is_empty() {
                            // Disconnect the oldest client to make room.
                            let (oldest_id, abort) = self.abort_senders.remove(0);
                            tracing::info!(
                                id = oldest_id,
                                addr = ?remote_addr,
                                "evicting oldest VNC client for new connection"
                            );
                            abort.send(());
                            self.dirty_senders.retain(|(cid, _, _)| *cid != oldest_id);
                        } else {
                            // Drop the socket to close the connection immediately.
                            tracing::warn!(
                                addr = ?remote_addr,
                                max = self.max_clients,
                                "VNC client rejected, limit reached"
                            );
                            continue;
                        }
                    }
                    let sock: socket2::Socket = socket.into();
                    let _ = sock.set_tcp_nodelay(true);
                    match PolledSocket::new(driver, sock) {
                        Ok(socket) => self.spawn_client(driver, socket, remote_addr),
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                "failed to register VNC client socket, dropping connection"
                            );
                        }
                    }
                }
                Event::ClientDone(id) => {
                    self.abort_senders.retain(|(cid, _)| *cid != id);
                    self.dirty_senders.retain(|(cid, _, _)| *cid != id);
                    tracing::info!(id, count = self.clients.len(), "VNC client disconnected");
                }
                Event::DirtyRects(rects) => {
                    if !device_dirty_seen {
                        device_dirty_seen = true;
                        tracing::info!("device dirty rects active, preferring over tile diff");
                    }
                    // Broadcast to all connected clients. Arc avoids cloning
                    // the rect Vec for each client (only ref-count bump).
                    let rects = Arc::new(rects);
                    for (cid, sender, missed) in &mut self.dirty_senders {
                        if sender.try_send(Arc::clone(&rects)).is_err() {
                            missed.store(true, Ordering::Relaxed);
                            tracing::debug!(
                                id = cid,
                                "client dirty channel full, flagged for full refresh"
                            );
                        }
                    }
                    tracing::trace!(
                        rect_count = rects.len(),
                        clients = self.dirty_senders.len(),
                        "broadcast device dirty rects"
                    );
                }
            }
        }
    }

    /// Creates a new client connection future and adds it to the active set.
    fn spawn_client(
        &mut self,
        driver: &LocalDriver,
        socket: PolledSocket<socket2::Socket>,
        remote_addr: impl std::fmt::Debug,
    ) {
        let id = self.next_client_id;
        self.next_client_id += 1;
        let addr_str = format!("{:?}", remote_addr);

        tracing::info!(
            id,
            addr = %addr_str,
            count = self.clients.len() + 1,
            "VNC client connected",
        );

        let view = self.view.clone();
        let input_send = self.input_send.clone();
        let (abort_send, abort_recv) = mesh::oneshot();
        // Per-client channel for receiving device dirty rects from the coordinator.
        // Capacity 4: enough to buffer a few batches without blocking the coordinator.
        #[expect(clippy::disallowed_methods)]
        let (dirty_send, dirty_recv) =
            futures::channel::mpsc::channel::<Arc<Vec<video_core::DirtyRect>>>(4);
        let missed_dirty = Arc::new(AtomicBool::new(false));
        self.dirty_senders
            .push((id, dirty_send, missed_dirty.clone()));

        // Each client gets its own VNC server instance with independent
        // zlib state and pixel format, sharing only the framebuffer and
        // input channel. The first frame is always a full screen refresh
        // (force_full_update=true in run_internal), so new clients don't
        // need to receive prior device dirty rects.
        let driver = driver.clone();
        let client_future = Box::pin(async move {
            let fb = SharedView(view);
            let input = SharedInput(input_send);
            let mut vncserver = vnc::Server::new(
                "OpenVMM VM".into(),
                socket,
                fb,
                input,
                Some(dirty_recv),
                Some(missed_dirty),
            );
            let mut updater = vncserver.updater();

            let mut timer = PolledTimer::new(&driver);
            let update_task = async {
                loop {
                    timer.sleep(Duration::from_millis(30)).await;
                    updater.update();
                }
            };

            let r = futures::select! { // race semantics
                r = vncserver.run().fuse() => r.context("VNC error"),
                _ = abort_recv.fuse() => Err(anyhow!("VNC connection aborted")),
                _ = update_task.fuse() => unreachable!(),
            };
            match r {
                Ok(_) => {}
                Err(err) => tracing::error!(error = err.as_error(), id, "VNC client error"),
            }
            id
        });

        // Store the abort sender separately so we can drop it to cancel
        // the client on shutdown without needing to drive the future.
        self.abort_senders.push((id, abort_send));
        self.clients.push(client_future);
    }

    /// Aborts all active clients and waits for them to finish.
    async fn abort_all_clients(&mut self) {
        // Drop all abort senders, which closes their oneshot channels and
        // causes each client's abort_recv to resolve.
        self.abort_senders.clear();
        self.dirty_senders.clear();
        // Drive all client futures to completion so they can clean up.
        while self.clients.next().await.is_some() {}
    }
}

impl<T: Listener> inspect::Inspect for MultiClientServer<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.display_debug("local_addr", &self.listener.get().local_addr().unwrap());
        resp.field("client_count", self.clients.len());
        resp.field("has_dirty_recv", self.dirty_recv.is_some());
    }
}

/// Wrapper around `mesh::Sender<InputData>` that implements `vnc::Input`.
///
/// Each client gets its own clone; input from any client goes to the same VM.
struct SharedInput(mesh::Sender<InputData>);

impl vnc::Input for SharedInput {
    fn key(&mut self, scancode: u16, is_down: bool) {
        self.0.send(InputData::Keyboard(KeyboardData {
            code: scancode,
            make: is_down,
        }));
    }

    fn mouse(&mut self, button_mask: u8, x: u16, y: u16) {
        self.0
            .send(InputData::Mouse(MouseData { button_mask, x, y }));
    }
}

/// Wrapper around `Arc<Mutex<ViewWrapper>>` that implements `vnc::Framebuffer`.
///
/// The mutex is needed because `View::resolution()` mutates internal state
/// (drains a channel). Lock durations are trivially short (memory reads).
struct SharedView(Arc<Mutex<ViewWrapper>>);

impl vnc::Framebuffer for SharedView {
    fn read_line(&mut self, line: u16, data: &mut [u8]) {
        self.0.lock().0.read_line(line, data)
    }

    fn resolution(&mut self) -> (u16, u16) {
        self.0.lock().0.resolution()
    }
}

#[derive(Debug)]
struct ViewWrapper(framebuffer::View);

#[cfg(test)]
mod tests {
    use super::*;
    use framebuffer::FRAMEBUFFER_SIZE;
    use futures::FutureExt;
    use input_core::InputData;
    use sparse_mmap::SparseMapping;
    use sparse_mmap::alloc_shared_memory;
    use std::io::Read;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::thread;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use video_core::DirtyRect;
    use video_core::FramebufferFormat;

    const ENCODING_TYPE_RAW: u32 = 0;
    const ENCODING_TYPE_DESKTOP_SIZE: u32 = -223i32 as u32;

    #[derive(Debug)]
    struct UpdateRect {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        encoding: u32,
        payload: Vec<u8>,
    }

    struct Client {
        stream: TcpStream,
        width: u16,
        height: u16,
    }

    impl Client {
        fn connect(addr: SocketAddr) -> Self {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let mut version = [0; 12];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            stream.write_all(b"RFB 003.008\n").unwrap();

            let mut sec_count = [0; 1];
            stream.read_exact(&mut sec_count).unwrap();
            assert_eq!(sec_count, [1]);
            let mut sec_types = vec![0; sec_count[0] as usize];
            stream.read_exact(&mut sec_types).unwrap();
            assert_eq!(sec_types, [1]);
            stream.write_all(&[1]).unwrap();

            let mut sec_result = [0; 4];
            stream.read_exact(&mut sec_result).unwrap();
            assert_eq!(sec_result, [0; 4]);
            stream.write_all(&[1]).unwrap();

            let mut init = [0; 24];
            stream.read_exact(&mut init).unwrap();
            let width = u16::from_be_bytes([init[0], init[1]]);
            let height = u16::from_be_bytes([init[2], init[3]]);
            let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
            let mut name = vec![0; name_len];
            stream.read_exact(&mut name).unwrap();
            assert_eq!(name, b"OpenVMM VM");

            Self {
                stream,
                width,
                height,
            }
        }

        fn send_update_request(&mut self, incremental: bool) {
            let mut request = [0; 10];
            request[0] = 3;
            request[1] = u8::from(incremental);
            request[6..8].copy_from_slice(&self.width.to_be_bytes());
            request[8..10].copy_from_slice(&self.height.to_be_bytes());
            self.stream.write_all(&request).unwrap();
        }

        fn send_pointer_event(&mut self, button_mask: u8, x: u16, y: u16) {
            let mut request = [0; 6];
            request[0] = 5;
            request[1] = button_mask;
            request[2..4].copy_from_slice(&x.to_be_bytes());
            request[4..6].copy_from_slice(&y.to_be_bytes());
            self.stream.write_all(&request).unwrap();
        }

        fn send_set_encodings(&mut self, encodings: &[u32]) {
            let mut request = [0; 4];
            request[0] = 2;
            request[2..4].copy_from_slice(&(encodings.len() as u16).to_be_bytes());
            self.stream.write_all(&request).unwrap();
            for &encoding in encodings {
                self.stream.write_all(&encoding.to_be_bytes()).unwrap();
            }
        }

        fn read_update(&mut self) -> Vec<UpdateRect> {
            self.try_read_update(Duration::from_secs(5))
                .expect("timed out waiting for framebuffer update")
        }

        fn try_read_update(&mut self, timeout: Duration) -> Option<Vec<UpdateRect>> {
            self.stream.set_read_timeout(Some(timeout)).unwrap();
            let mut header = [0; 4];
            match self.stream.read_exact(&mut header) {
                Ok(()) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return None;
                }
                Err(err) => panic!("failed to read framebuffer update header: {err}"),
            }
            assert_eq!(header[0], 0);
            let rect_count = u16::from_be_bytes([header[2], header[3]]) as usize;
            let mut rects = Vec::with_capacity(rect_count);
            for _ in 0..rect_count {
                let mut rect = [0; 12];
                self.stream.read_exact(&mut rect).unwrap();
                let x = u16::from_be_bytes([rect[0], rect[1]]);
                let y = u16::from_be_bytes([rect[2], rect[3]]);
                let width = u16::from_be_bytes([rect[4], rect[5]]);
                let height = u16::from_be_bytes([rect[6], rect[7]]);
                let encoding = u32::from_be_bytes([rect[8], rect[9], rect[10], rect[11]]);
                let payload_len = match encoding {
                    ENCODING_TYPE_RAW => width as usize * height as usize * 4,
                    ENCODING_TYPE_DESKTOP_SIZE => {
                        self.width = width;
                        self.height = height;
                        0
                    }
                    other => panic!("unsupported test encoding {other:#x}"),
                };
                let mut payload = vec![0; payload_len];
                self.stream.read_exact(&mut payload).unwrap();
                rects.push(UpdateRect {
                    x,
                    y,
                    width,
                    height,
                    encoding,
                    payload,
                });
            }
            self.stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            Some(rects)
        }
    }

    struct WorkerServer {
        addr: SocketAddr,
        vram: SparseMapping,
        format_send: mesh::Sender<FramebufferFormat>,
        input_recv: mesh::Receiver<InputData>,
        dirty_send: Option<mesh::Sender<Vec<DirtyRect>>>,
        stop_send: Option<mesh::OneshotSender<()>>,
        join: Option<JoinHandle<anyhow::Result<()>>>,
    }

    impl WorkerServer {
        fn stop(mut self) {
            if let Some(stop) = self.stop_send.take() {
                stop.send(());
            }
            if let Some(join) = self.join.take() {
                let result = join.join().unwrap();
                assert!(result.is_ok(), "{result:?}");
            }
        }
    }

    fn pixel(r: u8, g: u8, b: u8) -> [u8; 4] {
        ((r as u32) << 16 | (g as u32) << 8 | b as u32).to_le_bytes()
    }

    fn start_server(width: u16, height: u16, with_dirty: bool) -> WorkerServer {
        start_server_with_options(width, height, with_dirty, 16, false)
    }

    fn start_server_with_max_clients(
        width: u16,
        height: u16,
        with_dirty: bool,
        max_clients: usize,
    ) -> WorkerServer {
        start_server_with_options(width, height, with_dirty, max_clients, false)
    }

    fn start_server_with_options(
        width: u16,
        height: u16,
        with_dirty: bool,
        max_clients: usize,
        evict_oldest: bool,
    ) -> WorkerServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let vram = alloc_shared_memory(FRAMEBUFFER_SIZE, "vnc-worker-test").unwrap();
        let writer_vram = vram.try_clone().unwrap();
        let (fb, access) = framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).unwrap();
        let format_send = fb.format_send();
        format_send.send(FramebufferFormat {
            width: width as usize,
            height: height as usize,
            bytes_per_line: width as usize * 4,
            offset: 0,
        });

        let mapping = SparseMapping::new(FRAMEBUFFER_SIZE).unwrap();
        mapping
            .map_file(0, FRAMEBUFFER_SIZE, &writer_vram, 0, true)
            .unwrap();

        let (input_send, input_recv) = mesh::channel();
        let (dirty_send, dirty_recv) = if with_dirty {
            let (send, recv) = mesh::channel();
            (Some(send), Some(recv))
        } else {
            (None, None)
        };
        let (stop_send, stop_recv) = mesh::oneshot();

        let join = thread::spawn(move || {
            block_with_io(async |driver| -> anyhow::Result<()> {
                let mut server = MultiClientServer {
                    listener: PolledSocket::new(&driver, listener)?,
                    view: Arc::new(Mutex::new(ViewWrapper(access.view().unwrap()))),
                    input_send,
                    dirty_recv,
                    dirty_senders: Vec::new(),
                    clients: unicycle::FuturesUnordered::new(),
                    abort_senders: Vec::new(),
                    next_client_id: 0,
                    max_clients,
                    evict_oldest,
                };

                futures::select! {
                    result = server.process(&driver).fuse() => result,
                    _ = stop_recv.fuse() => Ok(()),
                }
            })
        });

        WorkerServer {
            addr,
            vram: mapping,
            format_send,
            input_recv,
            dirty_send,
            stop_send: Some(stop_send),
            join: Some(join),
        }
    }

    fn wait_for_input(recv: &mut mesh::Receiver<InputData>) -> InputData {
        for _ in 0..100 {
            if let Ok(data) = recv.try_recv() {
                return data;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for input");
    }

    #[test]
    fn e2e_multiclient_broadcasts_updates_and_forwards_input() {
        let mut server = start_server(32, 32, true);
        let mut client1 = Client::connect(server.addr);
        let mut client2 = Client::connect(server.addr);

        client1.send_update_request(false);
        let first = client1.read_update();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].width, 32);
        assert_eq!(first[0].height, 32);
        assert_eq!(first[0].encoding, 0);

        client2.send_update_request(false);
        let second = client2.read_update();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].width, 32);
        assert_eq!(second[0].height, 32);
        assert_eq!(second[0].encoding, 0);

        let pixel_offset = (20usize * 32 + 20) * 4;
        server
            .vram
            .write_at(pixel_offset, &pixel(0x12, 0x34, 0x56))
            .unwrap();
        server.dirty_send.as_ref().unwrap().send(vec![DirtyRect {
            left: 16,
            top: 16,
            right: 32,
            bottom: 32,
        }]);

        client1.send_update_request(true);
        let update1 = client1.read_update();
        assert_eq!(update1.len(), 1);
        assert_eq!(update1[0].x, 16);
        assert_eq!(update1[0].y, 16);
        assert_eq!(update1[0].width, 16);
        assert_eq!(update1[0].height, 16);
        assert_eq!(update1[0].encoding, 0);
        assert_eq!(update1[0].payload.len(), 16 * 16 * 4);

        client2.send_update_request(true);
        let update2 = client2.read_update();
        assert_eq!(update2.len(), 1);
        assert_eq!(update2[0].x, 16);
        assert_eq!(update2[0].y, 16);
        assert_eq!(update2[0].width, 16);
        assert_eq!(update2[0].height, 16);
        assert_eq!(update2[0].encoding, 0);
        assert_eq!(update2[0].payload.len(), 16 * 16 * 4);

        client1.send_pointer_event(1, 31, 31);
        match wait_for_input(&mut server.input_recv) {
            InputData::Mouse(MouseData {
                button_mask: 1,
                x: 0x7fff,
                y: 0x7fff,
            }) => {}
            other => panic!("unexpected input event: {other:?}"),
        }

        drop(client1);
        drop(client2);
        server.stop();
    }

    #[test]
    fn e2e_dirty_channel_close_falls_back_to_tile_diff() {
        let mut server = start_server(32, 32, true);
        let mut client = Client::connect(server.addr);

        client.send_update_request(false);
        let initial = client.read_update();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].width, 32);
        assert_eq!(initial[0].height, 32);

        let first_offset = (2usize * 32 + 2) * 4;
        server
            .vram
            .write_at(first_offset, &pixel(0xaa, 0xbb, 0xcc))
            .unwrap();
        server.dirty_send.as_ref().unwrap().send(vec![DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]);
        client.send_update_request(true);
        let dirty_update = client.read_update();
        assert_eq!(dirty_update.len(), 1);
        assert_eq!(dirty_update[0].x, 0);
        assert_eq!(dirty_update[0].y, 0);
        assert_eq!(dirty_update[0].width, 16);
        assert_eq!(dirty_update[0].height, 16);

        let second_offset = (20usize * 32 + 20) * 4;
        drop(server.dirty_send.take());
        server
            .vram
            .write_at(second_offset, &pixel(0x11, 0x22, 0x33))
            .unwrap();
        let mut fallback_update = None;
        for _ in 0..20 {
            client.send_update_request(true);
            if let Some(update) = client.try_read_update(Duration::from_millis(50)) {
                if update.len() == 1
                    && update[0].encoding == ENCODING_TYPE_RAW
                    && update[0].x == 16
                    && update[0].y == 16
                    && update[0].width == 16
                    && update[0].height == 16
                {
                    fallback_update = Some(update);
                    break;
                }
            }
        }
        let fallback_update = fallback_update.expect("dirty-close fallback update not observed");
        assert_eq!(fallback_update.len(), 1);

        drop(client);
        server.stop();
    }

    #[test]
    fn e2e_rejects_connections_over_limit() {
        let server = start_server(1, 1, false);
        let mut accepted = Vec::new();
        for _ in 0..16 {
            let mut stream = TcpStream::connect(server.addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut version = [0; 12];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            accepted.push(stream);
        }

        let mut rejected = TcpStream::connect(server.addr).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut version = [0; 12];
        let read = rejected.read(&mut version);
        assert!(matches!(read, Ok(0) | Err(_)));

        drop(rejected);
        drop(accepted);
        server.stop();
    }

    #[test]
    fn e2e_max_clients_1_allows_single_connection() {
        let server = start_server_with_max_clients(4, 4, false, 1);
        let mut client = Client::connect(server.addr);

        // First client works.
        client.send_update_request(false);
        let update = client.read_update();
        assert!(!update.is_empty());

        // Second client is rejected.
        let mut rejected = TcpStream::connect(server.addr).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 12];
        let read = rejected.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)));

        drop(rejected);
        drop(client);
        server.stop();
    }

    #[test]
    fn e2e_max_clients_custom_limit_accepts_up_to_limit() {
        let limit = 3;
        let server = start_server_with_max_clients(4, 4, false, limit);
        let mut accepted = Vec::new();

        // Connect exactly `limit` clients.
        for _ in 0..limit {
            let mut stream = TcpStream::connect(server.addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut version = [0; 12];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            accepted.push(stream);
        }

        // The (limit+1)th client is rejected.
        let mut rejected = TcpStream::connect(server.addr).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 12];
        let read = rejected.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)));

        drop(rejected);
        drop(accepted);
        server.stop();
    }

    #[test]
    fn e2e_max_clients_slot_freed_after_disconnect() {
        let server = start_server_with_max_clients(4, 4, false, 1);
        let mut client1 = Client::connect(server.addr);
        client1.send_update_request(false);
        let _ = client1.read_update();

        // Disconnect first client.
        drop(client1);
        // Give the server time to reap the disconnected client.
        thread::sleep(Duration::from_millis(100));

        // A new client can now connect.
        let mut client2 = Client::connect(server.addr);
        client2.send_update_request(false);
        let update = client2.read_update();
        assert!(!update.is_empty());

        drop(client2);
        server.stop();
    }

    #[test]
    fn e2e_evict_oldest_disconnects_first_client() {
        let server = start_server_with_options(4, 4, false, 1, true);

        // Connect client A — should work.
        let mut client_a = Client::connect(server.addr);
        client_a.send_update_request(false);
        let _ = client_a.read_update();

        // Connect client B — should evict client A.
        let mut client_b = Client::connect(server.addr);

        // Client A should be disconnected.
        client_a
            .stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 1];
        let read = client_a.stream.read(&mut buf);
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "client A should be disconnected"
        );

        // Client B should work.
        client_b.send_update_request(false);
        let update = client_b.read_update();
        assert!(!update.is_empty());

        drop(client_a);
        drop(client_b);
        server.stop();
    }

    #[test]
    fn e2e_evict_oldest_false_rejects_new_client() {
        let server = start_server_with_options(4, 4, false, 1, false);

        // Connect client A.
        let mut client_a = Client::connect(server.addr);
        client_a.send_update_request(false);
        let _ = client_a.read_update();

        // Client B should be rejected.
        let mut rejected = TcpStream::connect(server.addr).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 12];
        let read = rejected.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)));

        // Client A should still work — send a non-incremental request
        // to force a full update (incremental with no changes produces
        // no response).
        client_a.send_update_request(false);
        let update = client_a.read_update();
        assert!(!update.is_empty());

        drop(rejected);
        drop(client_a);
        server.stop();
    }

    #[test]
    fn e2e_evict_oldest_with_multiple_clients() {
        let server = start_server_with_options(4, 4, false, 2, true);

        // Connect A and B.
        let mut client_a = Client::connect(server.addr);
        client_a.send_update_request(false);
        let _ = client_a.read_update();

        let mut client_b = Client::connect(server.addr);
        client_b.send_update_request(false);
        let _ = client_b.read_update();

        // Connect C — should evict A (the oldest).
        let mut client_c = Client::connect(server.addr);

        // Client A should be disconnected.
        client_a
            .stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 1];
        let read = client_a.stream.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)), "client A should be evicted");

        // Client B should still work (non-incremental to force response).
        client_b.send_update_request(false);
        let update_b = client_b.read_update();
        assert!(!update_b.is_empty());

        // Client C should work.
        client_c.send_update_request(false);
        let update_c = client_c.read_update();
        assert!(!update_c.is_empty());

        drop(client_a);
        drop(client_b);
        drop(client_c);
        server.stop();
    }

    #[test]
    fn e2e_resize_parses_desktop_size_before_full_refresh() {
        let server = start_server(4, 2, false);
        let mut client = Client::connect(server.addr);

        client.send_set_encodings(&[ENCODING_TYPE_DESKTOP_SIZE]);
        client.send_update_request(false);
        let initial = client.read_update();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].encoding, ENCODING_TYPE_RAW);

        server.format_send.send(FramebufferFormat {
            width: 6,
            height: 3,
            bytes_per_line: 6 * 4,
            offset: 0,
        });
        client.send_update_request(true);
        let resize = client.read_update();
        assert_eq!(resize.len(), 1);
        assert_eq!(resize[0].encoding, ENCODING_TYPE_DESKTOP_SIZE);
        assert_eq!(resize[0].width, 6);
        assert_eq!(resize[0].height, 3);

        let refresh = client.read_update();
        assert_eq!(refresh.len(), 1);
        assert_eq!(refresh[0].encoding, ENCODING_TYPE_RAW);
        assert_eq!(refresh[0].width, 6);
        assert_eq!(refresh[0].height, 3);

        drop(client);
        server.stop();
    }
}
