// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! vhost-user backend device server.
//!
//! Implements the vhost-user backend protocol, driving a
//! `VirtioDevice` implementation. The server listens on
//! a Unix domain socket, accepts one connection at a time, and translates
//! vhost-user protocol messages into `VirtioDevice` trait calls.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]

pub mod memory;
pub mod queue_setup;

/// Re-export protocol types from the shared crate.
pub use vhost_user_protocol::protocol;
/// Re-export socket types from the shared crate.
pub use vhost_user_protocol::socket;

use crate::memory::MemoryRegionInfo;
use crate::memory::build_guest_memory;
use crate::protocol::*;
use crate::queue_setup::QueueSetup;
use crate::socket::SocketError;
use crate::socket::VhostUserSocket;
use anyhow::Context as _;
use guestmem::GuestMemory;
use pal_async::driver::SpawnDriver;
use pal_async::socket::PolledSocket;
use pal_event::Event;
use std::os::fd::OwnedFd;
use std::path::Path;
use unix_socket::UnixListener;
use virtio::DeviceTraits;
use virtio::DynVirtioDevice;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::interrupt::Interrupt;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// vhost-user backend device server.
///
/// Owns a `VirtioDevice` and serves the vhost-user protocol over a Unix
/// domain socket.
pub struct VhostUserDeviceServer {
    device: Box<dyn DynVirtioDevice>,
    /// The guest memory passed to the device on queue start.
    guest_memory: GuestMemory,
    /// Region metadata for VA→GPA translation of vring addresses.
    region_info: Vec<MemoryRegionInfo>,
}

impl VhostUserDeviceServer {
    /// Create a new server wrapping the given device.
    pub fn new(device: Box<dyn DynVirtioDevice>) -> Self {
        Self {
            device,
            guest_memory: GuestMemory::empty(),
            region_info: Vec::new(),
        }
    }

    /// Listen on `path` and serve a single client connection.
    ///
    /// After the client disconnects, the server resets the device and
    /// returns `Ok(())`.
    pub async fn run(
        mut self,
        driver: &(impl SpawnDriver + ?Sized),
        path: &Path,
    ) -> anyhow::Result<()> {
        // Remove stale socket file if it exists.
        let _ = std::fs::remove_file(path);

        let std_listener = UnixListener::bind(path)?;
        let mut listener = PolledSocket::new(driver, std_listener)?;

        tracing::info!(path = %path.display(), "vhost-user server listening");

        let (stream, _addr) = listener.accept().await?;
        let polled = PolledSocket::new(driver, stream)?;
        let socket = VhostUserSocket::new(polled);

        tracing::info!("vhost-user client connected");

        match self.handle_connection(&socket).await {
            Ok(()) => {
                tracing::info!("vhost-user client disconnected");
            }
            Err(e) => {
                tracing::warn!(
                    error = &*e as &dyn std::error::Error,
                    "vhost-user connection error"
                );
            }
        }

        // Clean up device state.
        self.stop_all_queues().await;
        self.device.reset().await;
        Ok(())
    }

    /// Serve a single connection (used for testing with socketpairs).
    pub async fn serve_connection(mut self, socket: VhostUserSocket) -> anyhow::Result<()> {
        let result = self.handle_connection(&socket).await;
        self.stop_all_queues().await;
        self.device.reset().await;
        result
    }

    /// Handle a single client connection's message loop.
    async fn handle_connection(&mut self, socket: &VhostUserSocket) -> anyhow::Result<()> {
        let traits = self.device.traits();
        let mut state = ConnectionState::new(&traits);

        loop {
            let (hdr, payload, fds) = match socket.recv_message().await {
                Ok(msg) => msg,
                Err(SocketError::Closed) => return Ok(()),
                Err(e) => return Err(e.into()),
            };

            if !hdr.version_valid() {
                tracelimit::warn_ratelimited!("invalid vhost-user version flag");
                continue;
            }

            if let Err(e) = self
                .dispatch_message(socket, &mut state, &traits, &hdr, &payload, fds)
                .await
            {
                tracelimit::warn_ratelimited!(
                    error = &*e as &dyn std::error::Error,
                    request = ?hdr.code(),
                    "error handling vhost-user message"
                );
                // Send a NACK if the frontend requested a reply, so it
                // doesn't block forever waiting for an ACK.
                if state.protocol_features.reply_ack() && hdr.need_reply() {
                    let nack = VhostUserU64Msg { value: 1 };
                    let _ = send_reply(socket, &hdr, nack.as_bytes(), &[]).await;
                }
            }
        }
    }

    /// Dispatch a single protocol message.
    async fn dispatch_message(
        &mut self,
        socket: &VhostUserSocket,
        state: &mut ConnectionState,
        traits: &DeviceTraits,
        hdr: &VhostUserMsgHeader,
        payload: &[u8],
        fds: Vec<OwnedFd>,
    ) -> anyhow::Result<()> {
        let code = hdr.code();

        match code {
            VhostUserRequestCode::GET_FEATURES => {
                let features = traits
                    .device_features
                    .with_vhost_user_protocol_features(true);
                let reply_payload = VhostUserU64Msg {
                    value: features.into_bits(),
                };
                send_reply(socket, hdr, reply_payload.as_bytes(), &[]).await?;
            }

            VhostUserRequestCode::SET_FEATURES => {
                let msg = parse_payload::<VhostUserU64Msg>(payload)?;
                tracing::trace!(features = %format!("0x{:x}", msg.value), "SET_FEATURES");
                // SET_FEATURES may be sent while queues are active (e.g.,
                // VHOST_F_LOG_ALL may be toggled). We don't currently support
                // any features that require restarting queues on change, so
                // just storing the new value is correct.
                // Mask out the vhost-user protocol features bit — it's a
                // vhost-user control bit, not a virtio device feature.
                state.negotiated_features = VirtioDeviceFeatures::from_bits(msg.value)
                    .with_vhost_user_protocol_features(false);
                maybe_ack(socket, hdr, state).await?
            }

            VhostUserRequestCode::GET_PROTOCOL_FEATURES => {
                let pf = VhostUserProtocolFeatures::new()
                    .with_mq(true)
                    .with_reply_ack(true)
                    .with_config(true)
                    .with_reset_device(true);
                let reply_payload = VhostUserU64Msg {
                    value: pf.into_bits(),
                };
                send_reply(socket, hdr, reply_payload.as_bytes(), &[]).await?;
            }

            VhostUserRequestCode::SET_PROTOCOL_FEATURES => {
                let msg = parse_payload::<VhostUserU64Msg>(payload)?;
                state.protocol_features = VhostUserProtocolFeatures::from_bits(msg.value);
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::GET_QUEUE_NUM => {
                let reply_payload = VhostUserU64Msg {
                    value: traits.max_queues as u64,
                };
                send_reply(socket, hdr, reply_payload.as_bytes(), &[]).await?;
            }

            VhostUserRequestCode::SET_OWNER => {
                // Required by the vhost-user spec — the frontend must send
                // SET_OWNER before using any data-path messages. For a
                // single-connection backend like this one, there is no
                // ownership state to track, so we just ACK it.
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_MEM_TABLE => {
                self.handle_set_mem_table(state, payload, fds).await?;
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::GET_CONFIG => {
                let config_hdr = parse_payload::<VhostUserConfigHeader>(payload)?;
                let offset = config_hdr.offset;
                let size = config_hdr.size;
                // Clamp to the device's config space length.
                let dev_len = traits.device_register_length;
                if size > dev_len || offset > dev_len - size {
                    anyhow::bail!(
                        "GET_CONFIG out of range: offset={offset} size={size} dev_len={dev_len}"
                    );
                }
                let mut config_data = vec![0u8; size as usize];
                // Read device config registers 4 bytes at a time.
                let mut pos = 0u32;
                while pos < size {
                    let reg_offset = offset + pos;
                    let val = self.device.read_registers_u32(reg_offset as u16).await;
                    let remaining = (size - pos) as usize;
                    let bytes = val.to_le_bytes();
                    let copy_len = remaining.min(4);
                    config_data[pos as usize..pos as usize + copy_len]
                        .copy_from_slice(&bytes[..copy_len]);
                    pos += 4;
                }
                // Reply: config header + config data.
                let mut reply_body =
                    Vec::with_capacity(size_of::<VhostUserConfigHeader>() + config_data.len());
                reply_body.extend_from_slice(config_hdr.as_bytes());
                reply_body.extend_from_slice(&config_data);
                send_reply(socket, hdr, &reply_body, &[]).await?;
            }

            VhostUserRequestCode::SET_CONFIG => {
                let config_hdr = parse_payload::<VhostUserConfigHeader>(payload)?;
                let dev_len = traits.device_register_length;
                if config_hdr.size > dev_len || config_hdr.offset > dev_len - config_hdr.size {
                    anyhow::bail!(
                        "SET_CONFIG out of range: offset={} size={} dev_len={dev_len}",
                        config_hdr.offset,
                        config_hdr.size,
                    );
                }
                let config_hdr_size = size_of::<VhostUserConfigHeader>();
                let config_data = payload.get(config_hdr_size..).unwrap_or(&[]);
                if config_data.len() < config_hdr.size as usize {
                    anyhow::bail!(
                        "SET_CONFIG payload too short: expected {} bytes, got {}",
                        config_hdr.size,
                        config_data.len(),
                    );
                }
                // Write device config registers 4 bytes at a time.
                let mut pos = 0u32;
                while pos < config_hdr.size {
                    let remaining = (config_hdr.size - pos) as usize;
                    let copy_len = remaining.min(4);
                    let mut bytes = [0u8; 4];
                    let data_start = pos as usize;
                    let data_end = data_start + copy_len;
                    if data_end <= config_data.len() {
                        bytes[..copy_len].copy_from_slice(&config_data[data_start..data_end]);
                    }
                    let val = u32::from_le_bytes(bytes);
                    self.device
                        .write_registers_u32((config_hdr.offset + pos) as u16, val)
                        .await;
                    pos += 4;
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_NUM => {
                let msg = parse_payload::<VhostUserVringState>(payload)?;
                let idx = msg.index as usize;
                if let Some(q) = state.queues.get_mut(idx) {
                    q.set_num(msg.num as u16);
                } else {
                    tracelimit::warn_ratelimited!(idx, "SET_VRING_NUM: invalid queue index");
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_ADDR => {
                let msg = parse_payload::<VhostUserVringAddr>(payload)?;
                let idx = msg.index as usize;
                tracing::trace!(
                    idx,
                    desc = %format!("0x{:x}", msg.desc_user_addr),
                    avail = %format!("0x{:x}", msg.avail_user_addr),
                    used = %format!("0x{:x}", msg.used_user_addr),
                    "SET_VRING_ADDR",
                );
                if let Some(q) = state.queues.get_mut(idx) {
                    let desc_gpa = memory::va_to_gpa(&self.region_info, msg.desc_user_addr)
                        .context("SET_VRING_ADDR desc")?;
                    let avail_gpa = memory::va_to_gpa(&self.region_info, msg.avail_user_addr)
                        .context("SET_VRING_ADDR avail")?;
                    let used_gpa = memory::va_to_gpa(&self.region_info, msg.used_user_addr)
                        .context("SET_VRING_ADDR used")?;
                    q.set_addr(desc_gpa, avail_gpa, used_gpa);
                } else {
                    tracelimit::warn_ratelimited!(idx, "SET_VRING_ADDR: invalid queue index");
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_BASE => {
                let msg = parse_payload::<VhostUserVringState>(payload)?;
                let idx = msg.index as usize;
                tracing::trace!(idx, base = %format!("0x{:x}", msg.num), "SET_VRING_BASE");
                if let Some(q) = state.queues.get_mut(idx) {
                    q.set_base(msg.num);
                } else {
                    tracelimit::warn_ratelimited!(idx, "SET_VRING_BASE: invalid queue index");
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::GET_VRING_BASE => {
                let msg = parse_payload::<VhostUserVringState>(payload)?;
                let idx = msg.index as usize;
                // Stop the queue and get its state. If the queue was
                // already stopped via SET_VRING_ENABLE(0), use the
                // saved state from that stop.
                let queue_state = if let Some(q) = state.queues.get_mut(idx) {
                    if q.is_active() {
                        let qs = stop_queue(&mut *self.device, idx as u16).await;
                        q.set_inactive();
                        qs.unwrap_or_default()
                    } else {
                        q.take_saved_state().unwrap_or_default()
                    }
                } else {
                    QueueState::default()
                };
                // For packed ring, pack both avail and used state into
                // the reply (avail in low 16, used in high 16). For split
                // ring, only the avail index matters.
                let num = if state.negotiated_features.ring_packed() {
                    (queue_state.avail_index as u32) | ((queue_state.used_index as u32) << 16)
                } else {
                    queue_state.avail_index as u32
                };
                let reply_payload = VhostUserVringState {
                    index: msg.index,
                    num,
                };
                send_reply(socket, hdr, reply_payload.as_bytes(), &[]).await?;
            }

            VhostUserRequestCode::SET_VRING_KICK => {
                let msg = parse_payload::<VhostUserU64Msg>(payload)?;
                let idx = (msg.value & VHOST_USER_VRING_INDEX_MASK) as usize;
                let nofd = msg.value & VHOST_USER_VRING_NOFD_MASK != 0;
                tracing::trace!(idx, nofd, fd_count = fds.len(), "SET_VRING_KICK");
                if !nofd
                    && let Some(fd) = fds.into_iter().next()
                    && let Some(q) = state.queues.get_mut(idx)
                {
                    q.set_kick(event_from_fd(fd));
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_CALL => {
                let msg = parse_payload::<VhostUserU64Msg>(payload)?;
                let idx = (msg.value & VHOST_USER_VRING_INDEX_MASK) as usize;
                let nofd = msg.value & VHOST_USER_VRING_NOFD_MASK != 0;
                tracing::trace!(idx, nofd, fd_count = fds.len(), "SET_VRING_CALL");
                if let Some(q) = state.queues.get_mut(idx) {
                    if nofd {
                        q.set_call(Interrupt::null());
                    } else if let Some(fd) = fds.into_iter().next() {
                        q.set_call(Interrupt::from_event(event_from_fd(fd)));
                    }
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_ERR => {
                // TODO: store the error eventfd and signal it on device errors.
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::SET_VRING_ENABLE => {
                let msg = parse_payload::<VhostUserVringState>(payload)?;
                let idx = msg.index as usize;
                let enable = msg.num != 0;
                tracing::trace!(idx, enable, "SET_VRING_ENABLE");
                if let Some(q) = state.queues.get_mut(idx) {
                    if enable {
                        if !q.is_active() {
                            if let Some((resources, raw_base)) =
                                q.try_activate(self.guest_memory.clone())
                            {
                                // Split raw_base into QueueState based on ring type.
                                let queue_state = if state.negotiated_features.ring_packed() {
                                    // Packed: low 16 = avail state, high 16 = used state.
                                    QueueState {
                                        avail_index: raw_base as u16,
                                        used_index: (raw_base >> 16) as u16,
                                    }
                                } else {
                                    // Split: only avail index from SET_VRING_BASE.
                                    // Used index is read from guest memory by the queue.
                                    QueueState {
                                        avail_index: raw_base as u16,
                                        used_index: 0,
                                    }
                                };
                                tracing::trace!(
                                    idx,
                                    avail_index = queue_state.avail_index,
                                    used_index = queue_state.used_index,
                                    desc_addr = %format!("0x{:x}", resources.params.desc_addr),
                                    avail_addr = %format!("0x{:x}", resources.params.avail_addr),
                                    used_addr = %format!("0x{:x}", resources.params.used_addr),
                                    size = resources.params.size,
                                    "activating queue",
                                );
                                self.device
                                    .start_queue(
                                        idx as u16,
                                        resources,
                                        &state.negotiated_features,
                                        Some(queue_state),
                                    )
                                    .await?;
                                q.set_active();
                            } else {
                                tracelimit::warn_ratelimited!(
                                    idx,
                                    "SET_VRING_ENABLE: queue not ready to activate"
                                );
                            }
                        }
                    } else if q.is_active() {
                        let state = stop_queue(&mut *self.device, idx as u16)
                            .await
                            .unwrap_or_default();
                        q.set_inactive_with_state(state);
                    }
                } else {
                    tracelimit::warn_ratelimited!(idx, "SET_VRING_ENABLE: invalid queue index");
                }
                maybe_ack(socket, hdr, state).await?;
            }

            VhostUserRequestCode::RESET_DEVICE => {
                self.stop_all_queues().await;
                self.device.reset().await;
                state.reset(&self.device.traits());
                maybe_ack(socket, hdr, state).await?;
            }

            _ => {
                tracelimit::warn_ratelimited!(
                    code = ?code,
                    "unhandled vhost-user request"
                );
                maybe_ack(socket, hdr, state).await?;
            }
        }

        Ok(())
    }

    /// Handle SET_MEM_TABLE: build new guest memory, stop/restart queues.
    async fn handle_set_mem_table(
        &mut self,
        state: &mut ConnectionState,
        payload: &[u8],
        fds: Vec<OwnedFd>,
    ) -> anyhow::Result<()> {
        let mem_hdr = parse_payload::<VhostUserMemoryHeader>(payload)?;
        let nregions = mem_hdr.nregions as usize;
        if nregions > VHOST_USER_MAX_FDS {
            anyhow::bail!(
                "SET_MEM_TABLE: nregions {} exceeds maximum {}",
                nregions,
                VHOST_USER_MAX_FDS
            );
        }
        let region_bytes = &payload[size_of::<VhostUserMemoryHeader>()..];
        let region_size = size_of::<VhostUserMemoryRegion>();

        if region_bytes.len() < nregions * region_size {
            anyhow::bail!(
                "SET_MEM_TABLE: expected {} region bytes, got {}",
                nregions * region_size,
                region_bytes.len()
            );
        }
        if fds.len() < nregions {
            anyhow::bail!(
                "SET_MEM_TABLE: expected {} fds, got {}",
                nregions,
                fds.len()
            );
        }

        // Parse regions and pair with fds.
        let mut regions = Vec::with_capacity(nregions);
        let mut fd_iter = fds.into_iter();
        for i in 0..nregions {
            let offset = i * region_size;
            let region =
                VhostUserMemoryRegion::read_from_bytes(&region_bytes[offset..offset + region_size])
                    .expect("region_size matches struct size");

            let fd = fd_iter.next().unwrap();
            tracing::trace!(
                idx = i,
                gpa = region.guest_phys_addr,
                size = region.memory_size,
                mmap_offset = region.mmap_offset,
                "SET_MEM_TABLE region",
            );
            regions.push((region, fd));
        }

        // Build the new GuestMemory first (non-destructive).
        let new_memory = build_guest_memory(regions)?;

        // Stop active queues, capturing their state for restart.
        let stopped = self.stop_active_queues_with_state(state).await;

        // Swap in the new memory and region info.
        self.guest_memory = new_memory.guest_memory;
        self.region_info = new_memory.regions;

        // Restart the queues that were active, with the new GuestMemory.
        for (idx, queue_state) in stopped {
            if let Some(q) = state.queues.get_mut(idx) {
                if let Some((resources, _raw_base)) = q.try_activate(self.guest_memory.clone()) {
                    match self
                        .device
                        .start_queue(
                            idx as u16,
                            resources,
                            &state.negotiated_features,
                            Some(queue_state),
                        )
                        .await
                    {
                        Ok(()) => {
                            q.set_active();
                        }
                        Err(e) => {
                            tracelimit::warn_ratelimited!(
                                idx,
                                error = &*e as &dyn std::error::Error,
                                "failed to restart queue after SET_MEM_TABLE"
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Stop all active queues, returning their indices and saved state.
    async fn stop_active_queues_with_state(
        &mut self,
        state: &mut ConnectionState,
    ) -> Vec<(usize, QueueState)> {
        let mut stopped = Vec::new();
        for (idx, q) in state.queues.iter_mut().enumerate() {
            if q.is_active() {
                let queue_state = stop_queue(&mut *self.device, idx as u16)
                    .await
                    .unwrap_or_default();
                q.set_inactive();
                stopped.push((idx, queue_state));
            }
        }
        stopped
    }

    /// Stop all active queues (cleanup).
    async fn stop_all_queues(&mut self) {
        // We don't have state.queues here, so ask the device to stop
        // all possible queues.
        let max_queues = self.device.traits().max_queues;
        for idx in 0..max_queues {
            // Try to stop; the device will return None if not active.
            let _ = stop_queue(&mut *self.device, idx).await;
        }
    }
}

/// Per-connection protocol state.
struct ConnectionState {
    negotiated_features: VirtioDeviceFeatures,
    protocol_features: VhostUserProtocolFeatures,
    queues: Vec<QueueSetup>,
}

impl ConnectionState {
    fn new(traits: &DeviceTraits) -> Self {
        let mut queues = Vec::with_capacity(traits.max_queues as usize);
        for _ in 0..traits.max_queues {
            queues.push(QueueSetup::new());
        }
        Self {
            negotiated_features: VirtioDeviceFeatures::new(),
            protocol_features: VhostUserProtocolFeatures::default(),
            queues,
        }
    }

    /// Reset device-level state. Connection-level state (protocol_features)
    /// is preserved — it was negotiated for the connection, not the device.
    fn reset(&mut self, traits: &DeviceTraits) {
        let Self {
            negotiated_features,
            protocol_features: _, // connection-level, not reset
            queues,
        } = self;
        *negotiated_features = VirtioDeviceFeatures::new();
        queues.clear();
        for _ in 0..traits.max_queues {
            queues.push(QueueSetup::new());
        }
    }
}

/// Stop a queue on the device and return its state.
async fn stop_queue(device: &mut dyn DynVirtioDevice, idx: u16) -> Option<QueueState> {
    device.stop_queue(idx).await
}

/// Send a reply for a GET_* message.
async fn send_reply(
    socket: &VhostUserSocket,
    request_hdr: &VhostUserMsgHeader,
    payload: &[u8],
    fds: &[OwnedFd],
) -> Result<(), SocketError> {
    let hdr = VhostUserMsgHeader::reply(request_hdr, payload.len() as u32);
    socket.send_message(&hdr, payload, fds).await
}

/// Send an ACK reply if REPLY_ACK was negotiated and NEED_REPLY is set.
async fn maybe_ack(
    socket: &VhostUserSocket,
    hdr: &VhostUserMsgHeader,
    state: &ConnectionState,
) -> Result<(), SocketError> {
    if state.protocol_features.reply_ack() && hdr.need_reply() {
        let reply_payload = VhostUserU64Msg { value: 0 };
        send_reply(socket, hdr, reply_payload.as_bytes(), &[]).await?;
    }
    Ok(())
}

/// Parse a payload as a zerocopy type.
fn parse_payload<T: FromBytes>(payload: &[u8]) -> anyhow::Result<T> {
    T::read_from_prefix(payload)
        .map(|(val, _rest)| val)
        .map_err(|_| {
            anyhow::anyhow!(
                "payload too small: expected >= {} bytes, got {}",
                size_of::<T>(),
                payload.len()
            )
        })
}

/// Create a `pal_event::Event` from an `OwnedFd`.
///
/// The fd should be an eventfd. On Linux, `pal_event::Event` wraps an eventfd.
fn event_from_fd(fd: OwnedFd) -> Event {
    Event::from(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::socket::PolledSocket;
    use std::os::fd::AsFd;

    use test_with_tracing::test;
    use unix_socket::UnixStream;
    use virtio::DeviceTraits;
    use virtio::DeviceTraitsSharedMemory;
    use virtio::QueueResources;
    use virtio::VirtioDevice;
    use virtio::queue::QueueState;
    use virtio::spec::VirtioDeviceFeatures;
    use zerocopy::IntoBytes;

    /// A mock VirtioDevice for testing the protocol adapter.
    struct MockDevice {
        traits: DeviceTraits,
        started_queues: Vec<u16>,
        stopped_queues: Vec<u16>,
    }

    impl MockDevice {
        fn new() -> Self {
            Self {
                traits: DeviceTraits {
                    device_id: virtio::spec::VirtioDeviceType::BLK,
                    device_features: VirtioDeviceFeatures::new(),
                    max_queues: 2,
                    device_register_length: 0,
                    shared_memory: DeviceTraitsSharedMemory::default(),
                },
                started_queues: Vec::new(),
                stopped_queues: Vec::new(),
            }
        }
    }

    impl InspectMut for MockDevice {
        fn inspect_mut(&mut self, _req: inspect::Request<'_>) {}
    }

    impl VirtioDevice for MockDevice {
        fn traits(&self) -> DeviceTraits {
            self.traits.clone()
        }

        async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
            0
        }

        async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

        async fn start_queue(
            &mut self,
            idx: u16,
            _resources: QueueResources,
            _features: &VirtioDeviceFeatures,
            _initial_state: Option<QueueState>,
        ) -> anyhow::Result<()> {
            self.started_queues.push(idx);
            Ok(())
        }

        async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
            self.stopped_queues.push(idx);
            Some(QueueState {
                avail_index: 0,
                used_index: 0,
            })
        }

        async fn reset(&mut self) {
            self.started_queues.clear();
            self.stopped_queues.clear();
        }
    }

    /// Helper: create a Unix socket pair for testing.
    fn socket_pair() -> (UnixStream, UnixStream) {
        let (a, b) = socket2::Socket::pair(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .expect("socketpair failed");
        let a: UnixStream = a.into();
        let b: UnixStream = b.into();
        (a, b)
    }

    /// Helper: send a vhost-user message on the frontend socket.
    async fn send_msg(
        socket: &VhostUserSocket,
        code: VhostUserRequestCode,
        payload: &[u8],
        fds: &[impl AsFd],
    ) {
        let hdr = VhostUserMsgHeader {
            request: code.0,
            flags: VHOST_USER_FLAG_VERSION,
            size: payload.len() as u32,
        };
        socket
            .send_message(&hdr, payload, fds)
            .await
            .expect("send failed");
    }

    /// Helper: send a message and expect a reply, returning reply payload.
    async fn send_and_recv(
        socket: &VhostUserSocket,
        code: VhostUserRequestCode,
        payload: &[u8],
    ) -> Vec<u8> {
        send_msg(socket, code, payload, &[] as &[OwnedFd]).await;
        let (hdr, reply_payload, _fds) = socket.recv_message().await.expect("recv reply failed");
        assert!(hdr.is_reply(), "expected reply flag");
        reply_payload
    }

    #[async_test]
    async fn test_protocol_handshake(driver: DefaultDriver) {
        let (frontend_stream, backend_stream) = socket_pair();

        let backend_polled = PolledSocket::new(&driver, backend_stream).unwrap();
        let backend_socket = VhostUserSocket::new(backend_polled);

        let server = VhostUserDeviceServer::new(Box::new(MockDevice::new()));

        // Frontend side: drive the handshake.
        let frontend_polled = PolledSocket::new(&driver, frontend_stream).unwrap();
        let frontend = VhostUserSocket::new(frontend_polled);

        let frontend_task = async {
            // GET_FEATURES
            let reply = send_and_recv(&frontend, VhostUserRequestCode::GET_FEATURES, &[]).await;
            let features_msg = VhostUserU64Msg::read_from_bytes(&reply).unwrap();
            assert!(
                VirtioDeviceFeatures::from_bits(features_msg.value).vhost_user_protocol_features(),
                "should advertise PROTOCOL_FEATURES"
            );

            // SET_FEATURES (must include PROTOCOL_FEATURES bit)
            let set_features = VhostUserU64Msg {
                value: VirtioDeviceFeatures::new()
                    .with_vhost_user_protocol_features(true)
                    .into_bits(),
            };
            send_msg(
                &frontend,
                VhostUserRequestCode::SET_FEATURES,
                set_features.as_bytes(),
                &[] as &[OwnedFd],
            )
            .await;

            // GET_PROTOCOL_FEATURES
            let reply =
                send_and_recv(&frontend, VhostUserRequestCode::GET_PROTOCOL_FEATURES, &[]).await;
            let pf_msg = VhostUserU64Msg::read_from_bytes(&reply).unwrap();
            let pf = VhostUserProtocolFeatures::from_bits(pf_msg.value);
            assert!(pf.mq());
            assert!(pf.config());

            // SET_PROTOCOL_FEATURES
            let set_pf = VhostUserU64Msg {
                value: VhostUserProtocolFeatures::new()
                    .with_mq(true)
                    .with_reply_ack(true)
                    .with_config(true)
                    .into_bits(),
            };
            send_msg(
                &frontend,
                VhostUserRequestCode::SET_PROTOCOL_FEATURES,
                set_pf.as_bytes(),
                &[] as &[OwnedFd],
            )
            .await;

            // GET_QUEUE_NUM
            let reply = send_and_recv(&frontend, VhostUserRequestCode::GET_QUEUE_NUM, &[]).await;
            let qn_msg = VhostUserU64Msg::read_from_bytes(&reply).unwrap();
            assert_eq!(qn_msg.value, 2); // MockDevice has 2 queues

            // SET_OWNER
            send_msg(
                &frontend,
                VhostUserRequestCode::SET_OWNER,
                &[],
                &[] as &[OwnedFd],
            )
            .await;

            // Disconnect by dropping the frontend socket.
            drop(frontend);
        };

        let (server_result, ()) =
            futures::join!(server.serve_connection(backend_socket), frontend_task,);
        server_result.expect("server should exit cleanly");
    }

    /// Helper: create a temp file of the given size and return it as an OwnedFd.
    fn make_memfd(size: usize) -> OwnedFd {
        use std::io::Write;

        let mut f = tempfile::tempfile().expect("tempfile failed");
        f.write_all(&vec![0u8; size]).expect("write failed");
        f.into()
    }

    /// Build a SET_MEM_TABLE payload for one region at GPA 0 of the given size.
    fn mem_table_payload(gpa: u64, size: u64, userspace_addr: u64) -> Vec<u8> {
        let nregions: u32 = 1;
        let padding: u32 = 0;
        let region = VhostUserMemoryRegion {
            guest_phys_addr: gpa,
            memory_size: size,
            userspace_addr,
            mmap_offset: 0,
        };
        let mut buf = Vec::new();
        buf.extend_from_slice(&nregions.to_le_bytes());
        buf.extend_from_slice(&padding.to_le_bytes());
        buf.extend_from_slice(region.as_bytes());
        buf
    }

    /// Helper: perform the standard handshake (no REPLY_ACK) on the frontend socket.
    async fn do_handshake(frontend: &VhostUserSocket) {
        // GET_FEATURES
        let reply = send_and_recv(frontend, VhostUserRequestCode::GET_FEATURES, &[]).await;
        let features_msg = VhostUserU64Msg::read_from_bytes(&reply).unwrap();
        assert!(VirtioDeviceFeatures::from_bits(features_msg.value).vhost_user_protocol_features());

        // SET_FEATURES
        let set_features = VhostUserU64Msg {
            value: VirtioDeviceFeatures::new()
                .with_vhost_user_protocol_features(true)
                .into_bits(),
        };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_FEATURES,
            set_features.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;

        // GET_PROTOCOL_FEATURES
        let _reply =
            send_and_recv(frontend, VhostUserRequestCode::GET_PROTOCOL_FEATURES, &[]).await;

        // SET_PROTOCOL_FEATURES (no REPLY_ACK — keeps things simple)
        let set_pf = VhostUserU64Msg {
            value: VhostUserProtocolFeatures::new()
                .with_mq(true)
                .with_config(true)
                .into_bits(),
        };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_PROTOCOL_FEATURES,
            set_pf.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;

        // SET_OWNER
        send_msg(
            frontend,
            VhostUserRequestCode::SET_OWNER,
            &[],
            &[] as &[OwnedFd],
        )
        .await;
    }

    /// Helper: send SET_MEM_TABLE with one region backed by a memfd.
    async fn send_mem_table(frontend: &VhostUserSocket, region_size: usize) {
        let fd = make_memfd(region_size);
        let payload = mem_table_payload(0, region_size as u64, 0x1000_0000);
        send_msg(
            frontend,
            VhostUserRequestCode::SET_MEM_TABLE,
            &payload,
            &[fd],
        )
        .await;
    }

    /// Helper: set up and enable queue 0 with dummy addresses and eventfds.
    async fn setup_and_enable_queue(frontend: &VhostUserSocket) {
        // SET_VRING_NUM
        let vring_num = VhostUserVringState { index: 0, num: 256 };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_NUM,
            vring_num.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;

        // SET_VRING_ADDR — addresses within our mapped region (GPA 0, size 1MB).
        let vring_addr = VhostUserVringAddr {
            index: 0,
            flags: 0,
            desc_user_addr: 0x1000_0000, // The userspace_addr we used in SET_MEM_TABLE
            avail_user_addr: 0x1000_1000,
            used_user_addr: 0x1000_2000,
            log_guest_addr: 0,
        };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_ADDR,
            vring_addr.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;

        // SET_VRING_BASE
        let vring_base = VhostUserVringState { index: 0, num: 0 };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_BASE,
            vring_base.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;

        // SET_VRING_KICK (with eventfd)
        let kick_event = Event::new();
        let kick_fd: OwnedFd = kick_event.into();
        let kick_msg = VhostUserU64Msg { value: 0 }; // index 0, no NOFD
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_KICK,
            kick_msg.as_bytes(),
            &[kick_fd],
        )
        .await;

        // SET_VRING_CALL (with eventfd)
        let call_event = Event::new();
        let call_fd: OwnedFd = call_event.into();
        let call_msg = VhostUserU64Msg { value: 0 }; // index 0, no NOFD
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_CALL,
            call_msg.as_bytes(),
            &[call_fd],
        )
        .await;

        // SET_VRING_ENABLE
        let enable_msg = VhostUserVringState { index: 0, num: 1 };
        send_msg(
            frontend,
            VhostUserRequestCode::SET_VRING_ENABLE,
            enable_msg.as_bytes(),
            &[] as &[OwnedFd],
        )
        .await;
    }

    /// Verify that sending SET_MEM_TABLE while a queue is active causes
    /// the backend to stop and restart the queue (not leave it stopped).
    #[async_test]
    async fn test_set_mem_table_restarts_active_queues(driver: DefaultDriver) {
        use std::sync::Arc;

        use parking_lot::Mutex;

        /// A mock that records start/stop calls behind a shared lock so
        /// we can inspect them after the server task completes.
        struct TrackingDevice {
            inner: MockDevice,
            log: Arc<Mutex<Vec<&'static str>>>,
        }

        impl InspectMut for TrackingDevice {
            fn inspect_mut(&mut self, req: inspect::Request<'_>) {
                self.inner.inspect_mut(req);
            }
        }

        impl VirtioDevice for TrackingDevice {
            fn traits(&self) -> DeviceTraits {
                VirtioDevice::traits(&self.inner)
            }

            async fn read_registers_u32(&mut self, offset: u16) -> u32 {
                VirtioDevice::read_registers_u32(&mut self.inner, offset).await
            }

            async fn write_registers_u32(&mut self, offset: u16, val: u32) {
                VirtioDevice::write_registers_u32(&mut self.inner, offset, val).await;
            }

            async fn start_queue(
                &mut self,
                idx: u16,
                resources: QueueResources,
                features: &VirtioDeviceFeatures,
                initial_state: Option<QueueState>,
            ) -> anyhow::Result<()> {
                self.log.lock().push("start");
                VirtioDevice::start_queue(&mut self.inner, idx, resources, features, initial_state)
                    .await
            }

            async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
                self.log.lock().push("stop");
                VirtioDevice::stop_queue(&mut self.inner, idx).await
            }

            async fn reset(&mut self) {
                self.log.lock().push("reset");
                VirtioDevice::reset(&mut self.inner).await;
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let device = TrackingDevice {
            inner: MockDevice::new(),
            log: log.clone(),
        };

        let (frontend_stream, backend_stream) = socket_pair();
        let backend_polled = PolledSocket::new(&driver, backend_stream).unwrap();
        let backend_socket = VhostUserSocket::new(backend_polled);
        let server = VhostUserDeviceServer::new(Box::new(device));

        let frontend_polled = PolledSocket::new(&driver, frontend_stream).unwrap();
        let frontend = VhostUserSocket::new(frontend_polled);

        let frontend_task = async {
            do_handshake(&frontend).await;

            // 1. Initial SET_MEM_TABLE (before queues are started).
            send_mem_table(&frontend, 0x10_0000).await;

            // 2. Set up and enable queue 0.
            setup_and_enable_queue(&frontend).await;

            // 3. Send another SET_MEM_TABLE while queue 0 is active.
            //    This simulates a runtime memory update (e.g., hotplug/balloon).
            //    The backend must stop and restart the queue — not leave it dead.
            send_mem_table(&frontend, 0x10_0000).await;

            // Sync barrier: GET_FEATURES requires a reply, so receiving
            // the reply guarantees the backend has processed SET_MEM_TABLE.
            let _ = send_and_recv(&frontend, VhostUserRequestCode::GET_FEATURES, &[]).await;

            drop(frontend);
        };

        let (server_result, ()) =
            futures::join!(server.serve_connection(backend_socket), frontend_task);
        server_result.expect("server should exit cleanly");

        let log = log.lock();
        // Expected sequence:
        //   start  — SET_VRING_ENABLE (initial queue start)
        //   stop   — SET_MEM_TABLE stops the active queue
        //   start  — SET_MEM_TABLE restarts it with new memory
        //   stop   — serve_connection cleanup (stop_all_queues)
        //   stop   — serve_connection cleanup (second queue, not active)
        //   reset  — serve_connection cleanup
        //
        // The critical assertion: after the first start, a stop is
        // followed by another start (the restart), not just a stop.
        let start_count = log.iter().filter(|&&s| s == "start").count();
        assert!(
            start_count >= 2,
            "queue should be started at least twice (initial + restart after SET_MEM_TABLE), \
             got {start_count}. log: {log:?}"
        );
        // The first three entries should be: start, stop, start
        assert_eq!(
            &log[..3],
            &["start", "stop", "start"],
            "expected start→stop→start sequence for queue restart. log: {log:?}"
        );
    }
}
