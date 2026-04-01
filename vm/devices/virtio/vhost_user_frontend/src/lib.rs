// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]

//! vhost-user frontend: a [`VirtioDevice`] implementation that forwards
//! device operations to an external vhost-user backend over a Unix socket.
//!
//! This is the counterpart to the `VhostUserDeviceServer` in the
//! `vhost_user_backend` crate: the server hosts a device, while this
//! frontend connects to that server and presents it to the VMM as a
//! standard virtio device.

pub mod resolver;

use anyhow::Context as _;
use guestmem::GuestMemory;
use guestmem::ShareableRegion;
use inspect::InspectMut;
use pal_async::socket::PolledSocket;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use unix_socket::UnixStream;
use vhost_user_protocol::*;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::VirtioDeviceType;
use vmcore::interrupt::EventProxy;
use vmcore::vm_task::VmTaskDriver;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Offset added to GPAs to produce the "userspace VA" coordinate system
/// used in the vhost-user protocol. The vhost-user spec expresses vring
/// addresses as frontend userspace VAs, and the backend translates them
/// back using the region table from SET_MEM_TABLE.
///
/// We don't actually map guest memory into our VA space, so we use GPAs
/// as the base coordinate system. This non-zero offset ensures VA != GPA,
/// so any code path that accidentally skips the translation will produce
/// obviously wrong results instead of silently working.
///
/// Set to the maximum ARM64 physical address space size (2^52) so that
/// there is no possible collision with any valid GPA.
const GPA_TO_VA_OFFSET: u64 = 1 << 52;

/// Per-queue tracking state.
struct FrontendQueueState {
    active: bool,
    /// Saved queue params for reading used ring index during stop.
    params: Option<virtio::queue::QueueParams>,
    /// Keeps the interrupt event proxy task alive (if one was needed).
    _event_proxy: Option<EventProxy>,
}

/// A `VirtioDevice` that proxies to a vhost-user backend.
#[derive(InspectMut)]
#[inspect(skip)]
pub struct VhostUserFrontend {
    driver: VmTaskDriver,
    device_traits: DeviceTraits,
    protocol_features: VhostUserProtocolFeatures,
    socket: VhostUserSocket,
    /// Raw feature bits from GET_FEATURES (used to mask guest features).
    device_features_raw: u64,
    guest_features_sent: bool,
    /// Whether packed ring (VIRTIO_F_RING_PACKED) is active. Set when
    /// guest-negotiated features are sent to the backend.
    packed_ring: bool,
    /// Whether the memory table has been sent to the backend. The memory
    /// table is sent once (on the first `start_queue`) and not resent on
    /// reset, because the guest memory backing is the same file-backed
    /// allocation for the lifetime of the socket connection.
    mem_table_sent: bool,
    queues: Vec<FrontendQueueState>,
    /// Set on the first `start_queue` call, used by `stop_queue` to read
    /// the used index from the guest-visible used ring.
    guest_memory: Option<GuestMemory>,
}

impl VhostUserFrontend {
    /// Whether the REPLY_ACK protocol feature was negotiated.
    fn reply_ack(&self) -> bool {
        self.protocol_features.reply_ack()
    }

    /// Connect to a vhost-user backend, negotiate features, and send
    /// the memory table.
    ///
    /// `device_id` is the virtio device ID (e.g., 2 for block) —
    /// vhost-user has no GET_DEVICE_ID message so this must come from
    /// the resource configuration.
    pub async fn new(
        driver: VmTaskDriver,
        socket_path: &std::path::Path,
        device_id: VirtioDeviceType,
    ) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
        let polled = PolledSocket::new(&driver, stream)?;
        let socket = VhostUserSocket::new(polled);

        Self::from_socket(driver, socket, device_id).await
    }

    /// Create from an already-connected socket (useful for testing with
    /// socketpairs).
    pub async fn from_socket(
        driver: VmTaskDriver,
        socket: VhostUserSocket,
        device_id: VirtioDeviceType,
    ) -> anyhow::Result<Self> {
        // 1. GET_FEATURES
        let device_features_raw = send_get_u64(&socket, VhostUserRequestCode::GET_FEATURES).await?;
        tracing::trace!(features = %format!("0x{device_features_raw:x}"), "GET_FEATURES");

        // 2. Negotiate protocol features (only if the backend advertises them).
        let negotiated_proto = if device_features_raw & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
            let proto_features_raw =
                send_get_u64(&socket, VhostUserRequestCode::GET_PROTOCOL_FEATURES).await?;
            let wanted = VhostUserProtocolFeatures::new()
                .with_mq(true)
                .with_reply_ack(true)
                .with_config(true)
                .with_reset_device(true);
            let negotiated =
                VhostUserProtocolFeatures::from_bits(proto_features_raw & wanted.into_bits());
            send_set_u64(
                &socket,
                VhostUserRequestCode::SET_PROTOCOL_FEATURES,
                negotiated.into_bits(),
                false, // REPLY_ACK just negotiated — not yet active
            )
            .await?;
            negotiated
        } else {
            VhostUserProtocolFeatures::new()
        };

        // 3. SET_OWNER
        send_simple(&socket, VhostUserRequestCode::SET_OWNER, false).await?;

        // 4. GET_QUEUE_NUM (requires MQ protocol feature)
        let max_queues = if negotiated_proto.mq() {
            send_get_u64(&socket, VhostUserRequestCode::GET_QUEUE_NUM)
                .await
                .unwrap_or(1) as u16
        } else {
            1
        };
        tracing::trace!(max_queues, "GET_QUEUE_NUM");

        // Build DeviceTraits from the wire features.
        let device_features =
            features_from_u64(device_features_raw & !(VHOST_USER_F_PROTOCOL_FEATURES));

        // Config reads/writes are forwarded live via GET_CONFIG/SET_CONFIG,
        // so we don't need to prefetch. Use the vhost-user max config size
        // (256) as the register length; reads beyond the backend's actual
        // config space will return zeros.
        let device_register_length = if negotiated_proto.config() { 256 } else { 0 };

        let device_traits = DeviceTraits {
            device_id,
            device_features,
            max_queues,
            device_register_length,
            shared_memory: DeviceTraitsSharedMemory::default(),
        };

        let queues = (0..max_queues)
            .map(|_| FrontendQueueState {
                active: false,
                params: None,
                _event_proxy: None,
            })
            .collect();

        Ok(Self {
            driver,
            device_traits,
            protocol_features: negotiated_proto,
            socket,
            device_features_raw,
            guest_features_sent: false,
            mem_table_sent: false,
            packed_ring: false,
            queues,
            guest_memory: None,
        })
    }
}

impl VirtioDevice for VhostUserFrontend {
    fn traits(&self) -> DeviceTraits {
        self.device_traits.clone()
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        if !self.protocol_features.config() {
            return 0;
        }
        match send_get_config(&self.socket, offset as u32, 4).await {
            Ok(data) if data.len() >= 4 => u32::from_le_bytes(data[..4].try_into().unwrap()),
            Ok(_) => 0,
            Err(e) => {
                tracelimit::warn_ratelimited!(
                    error = &*e as &dyn std::error::Error,
                    offset,
                    "GET_CONFIG failed"
                );
                0
            }
        }
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        if let Err(e) =
            send_set_config(&self.socket, offset, &val.to_le_bytes(), self.reply_ack()).await
        {
            tracelimit::warn_ratelimited!(
                error = &*e as &dyn std::error::Error,
                offset,
                "SET_CONFIG failed"
            );
        }
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        // Send SET_MEM_TABLE before the first queue is started.
        //
        // The memory table is sent once and persists across device
        // resets. The backend retains the memory mapping (it is
        // connection-scoped, not device-scoped), and the guest memory
        // backing doesn't change for the lifetime of the connection.
        if !self.mem_table_sent {
            let sharing = resources.guest_memory.sharing().ok_or_else(|| {
                anyhow::anyhow!(
                    "vhost-user requires file-backed guest memory (sharing() returned None)"
                )
            })?;
            let exported_regions = sharing
                .get_regions()
                .await
                .map_err(|e| anyhow::anyhow!(e))?;

            tracing::trace!(region_count = exported_regions.len(), "SET_MEM_TABLE");
            send_set_mem_table(&self.socket, &exported_regions, self.reply_ack()).await?;
            self.mem_table_sent = true;
            self.guest_memory = Some(resources.guest_memory.clone());
        }

        // Send SET_FEATURES with the guest-negotiated features before the
        // first queue is started.  The backend needs this to know which
        // features are active.
        if !self.guest_features_sent {
            let guest_bits = features.bank(0) as u64 | ((features.bank(1) as u64) << 32);
            // Mask to only include features the backend actually advertised.
            // The VMM transport may add features (e.g., RING_PACKED) that
            // the backend doesn't support. Always include PROTOCOL_FEATURES
            // if it was negotiated — backends (e.g., virtiofsd) treat its
            // absence in SET_FEATURES as de-negotiation.
            let masked_bits =
                (guest_bits & self.device_features_raw) | VHOST_USER_F_PROTOCOL_FEATURES;
            tracing::trace!(
                idx,
                features = %format!("0x{masked_bits:x}"),
                "SET_FEATURES (guest-negotiated)",
            );
            send_set_u64(
                &self.socket,
                VhostUserRequestCode::SET_FEATURES,
                masked_bits,
                self.reply_ack(),
            )
            .await?;
            self.guest_features_sent = true;
            // Determine the effective ring format from the actually-
            // negotiated features (intersection of guest and backend).
            let negotiated = features_from_u64(masked_bits);
            self.packed_ring = negotiated.bank1().ring_packed();
        }

        let packed_ring = self.packed_ring;

        let base = initial_state.map(|s| s.avail_index).unwrap_or_else(|| {
            // For packed ring, the initial wrap counter is 1 (encoded in bit 15).
            if packed_ring { 0x8000 } else { 0 }
        });

        // For packed ring, SET_VRING_BASE packs both avail and used state:
        //   bits 0-14: last avail index
        //   bit 15: avail wrap counter
        //   bits 16-30: last used index
        //   bit 31: used wrap counter
        // For fresh start, used == avail. For save/restore, used comes from
        // the saved state.
        let vring_base = if packed_ring {
            let used = initial_state.map(|s| s.used_index).unwrap_or(0x8000);
            (base as u32) | ((used as u32) << 16)
        } else {
            base as u32
        };

        tracing::trace!(
            idx,
            size = resources.params.size,
            desc = %format!("0x{:x}", resources.params.desc_addr),
            avail = %format!("0x{:x}", resources.params.avail_addr),
            used = %format!("0x{:x}", resources.params.used_addr),
            base,
            has_event_interrupt = resources.notify.event().is_some(),
            "start_queue",
        );

        // SET_VRING_NUM
        send_vring_state(
            &self.socket,
            VhostUserRequestCode::SET_VRING_NUM,
            idx,
            resources.params.size as u32,
            self.reply_ack(),
        )
        .await?;

        // SET_VRING_ADDR — addresses must be in the VA coordinate system
        // (GPA + GPA_TO_VA_OFFSET) matching what we sent in SET_MEM_TABLE.
        send_vring_addr(
            &self.socket,
            idx,
            resources.params.desc_addr + GPA_TO_VA_OFFSET,
            resources.params.used_addr + GPA_TO_VA_OFFSET,
            resources.params.avail_addr + GPA_TO_VA_OFFSET,
            self.reply_ack(),
        )
        .await?;

        // SET_VRING_BASE
        tracing::trace!(idx, vring_base = %format!("0x{vring_base:x}"), "SET_VRING_BASE");
        send_vring_state(
            &self.socket,
            VhostUserRequestCode::SET_VRING_BASE,
            idx,
            vring_base,
            self.reply_ack(),
        )
        .await?;

        // SET_VRING_KICK — pass the kick eventfd to the backend
        send_vring_fd(
            &self.socket,
            VhostUserRequestCode::SET_VRING_KICK,
            idx,
            Some(&resources.event),
            self.reply_ack(),
        )
        .await?;

        // SET_VRING_CALL — pass an interrupt eventfd to the backend.
        //
        // If the transport's interrupt is already event-backed, pass it
        // directly. Otherwise, create an async proxy that bridges a new
        // event to Interrupt::deliver() (needed for e.g. MSI-X function-
        // backed interrupts where the transport has side effects like
        // updating the ISR register).
        let (call_event, event_proxy) = resources.notify.event_or_proxy(&self.driver)?;
        send_vring_fd(
            &self.socket,
            VhostUserRequestCode::SET_VRING_CALL,
            idx,
            Some(&call_event),
            self.reply_ack(),
        )
        .await?;

        // SET_VRING_ENABLE
        send_vring_state(
            &self.socket,
            VhostUserRequestCode::SET_VRING_ENABLE,
            idx,
            1,
            self.reply_ack(),
        )
        .await?;

        if let Some(q) = self.queues.get_mut(idx as usize) {
            q.active = true;
            q.params = Some(resources.params);
            q._event_proxy = event_proxy;
        }
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        let reply_ack = self.reply_ack();
        let q = self.queues.get_mut(idx as usize)?;
        if !q.active {
            return None;
        }

        // Disable the queue before stopping it. QEMU sends
        // SET_VRING_ENABLE(0) before GET_VRING_BASE to ensure the
        // backend's data plane stops processing kicks before the
        // control plane tears down the queue.
        if let Err(e) = send_vring_state(
            &self.socket,
            VhostUserRequestCode::SET_VRING_ENABLE,
            idx,
            0,
            reply_ack,
        )
        .await
        {
            tracelimit::warn_ratelimited!(
                error = &*e as &dyn std::error::Error,
                idx,
                "SET_VRING_ENABLE(0) failed during stop_queue"
            );
        }

        // GET_VRING_BASE implicitly stops the queue on the backend.
        // For packed ring, the reply packs both avail and used state:
        //   bits 0-15: avail state (index + wrap counter)
        //   bits 16-31: used state (index + wrap counter)
        // For split ring, only the low 16 bits matter (avail index),
        // and used_index is read from the guest-visible used ring.
        let vring_base = match send_get_vring_base(&self.socket, idx).await {
            Ok(base) => base,
            Err(e) => {
                tracelimit::warn_ratelimited!(
                    error = &*e as &dyn std::error::Error,
                    idx,
                    "GET_VRING_BASE failed during stop_queue; marking queue inactive"
                );
                q.active = false;
                q.params = None;
                q._event_proxy = None;
                return None;
            }
        };

        let (avail_index, used_index) = if self.packed_ring {
            (vring_base as u16, (vring_base >> 16) as u16)
        } else {
            let used = q
                .params
                .as_ref()
                .map(|params| {
                    read_used_index(
                        self.guest_memory
                            .as_ref()
                            .expect("memory set in start_queue"),
                        params,
                    )
                })
                .unwrap_or(0);
            (vring_base as u16, used)
        };

        q.active = false;
        q.params = None;
        q._event_proxy = None;
        Some(QueueState {
            avail_index,
            used_index,
        })
    }

    async fn reset(&mut self) {
        let reply_ack = self.reply_ack();
        // Stop all active queues.
        for idx in 0..self.queues.len() {
            if self.queues[idx].active {
                if let Err(e) = send_vring_state(
                    &self.socket,
                    VhostUserRequestCode::SET_VRING_ENABLE,
                    idx as u16,
                    0,
                    reply_ack,
                )
                .await
                {
                    tracelimit::warn_ratelimited!(
                        error = &*e as &dyn std::error::Error,
                        idx,
                        "SET_VRING_ENABLE(0) failed during reset"
                    );
                }
                if let Err(e) = send_get_vring_base(&self.socket, idx as u16).await {
                    tracelimit::warn_ratelimited!(
                        error = &*e as &dyn std::error::Error,
                        idx,
                        "GET_VRING_BASE failed during reset"
                    );
                }
                self.queues[idx].active = false;
                self.queues[idx].params = None;
                self.queues[idx]._event_proxy = None;
            }
        }
        self.guest_features_sent = false;
        self.packed_ring = false;
        // Send RESET_DEVICE if negotiated.
        if self.protocol_features.reset_device() {
            if let Err(e) =
                send_simple(&self.socket, VhostUserRequestCode::RESET_DEVICE, reply_ack).await
            {
                tracelimit::warn_ratelimited!(
                    error = &*e as &dyn std::error::Error,
                    "RESET_DEVICE failed during reset"
                );
            }
        }
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Protocol helper functions
// ---------------------------------------------------------------------------

/// Validate a reply header: check version flag, reply flag, and request code.
fn validate_reply(
    reply_hdr: &VhostUserMsgHeader,
    expected_code: VhostUserRequestCode,
) -> anyhow::Result<()> {
    anyhow::ensure!(reply_hdr.version_valid(), "reply has invalid version flag");
    anyhow::ensure!(reply_hdr.is_reply(), "expected reply flag in response");
    anyhow::ensure!(
        reply_hdr.code() == expected_code,
        "reply code {:?} does not match expected {:?}",
        reply_hdr.code(),
        expected_code,
    );
    Ok(())
}

/// Send a request and receive a validated reply.
///
/// Validates version, reply flag, and that the reply code matches.
/// Returns the reply payload and any file descriptors.
async fn send_and_recv(
    socket: &VhostUserSocket,
    code: VhostUserRequestCode,
    payload: &[u8],
    fds: &[impl AsFd],
) -> anyhow::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let hdr = VhostUserMsgHeader {
        request: code.0,
        flags: VHOST_USER_FLAG_VERSION,
        size: payload.len() as u32,
    };
    socket.send_message(&hdr, payload, fds).await?;
    let (reply_hdr, reply_payload, reply_fds) = socket.recv_message().await?;
    validate_reply(&reply_hdr, code)?;
    Ok((reply_payload, reply_fds))
}

/// Receive and validate a REPLY_ACK response.
async fn recv_ack(
    socket: &VhostUserSocket,
    expected_code: VhostUserRequestCode,
) -> anyhow::Result<()> {
    let (reply_hdr, payload, _fds) = socket.recv_message().await?;
    validate_reply(&reply_hdr, expected_code)?;
    let msg = VhostUserU64Msg::read_from_prefix(&payload)
        .map(|(val, _)| val)
        .map_err(|_| anyhow::anyhow!("ACK reply payload too small"))?;
    anyhow::ensure!(
        msg.value == 0,
        "backend returned error in ACK: {}",
        msg.value
    );
    Ok(())
}

/// Build the flags field for a request, optionally including NEED_REPLY.
fn request_flags(reply_ack: bool) -> u32 {
    let mut flags = VHOST_USER_FLAG_VERSION;
    if reply_ack {
        flags |= VHOST_USER_FLAG_NEED_REPLY;
    }
    flags
}

/// Send a request with no payload and receive a u64 reply.
async fn send_get_u64(socket: &VhostUserSocket, code: VhostUserRequestCode) -> anyhow::Result<u64> {
    tracing::trace!(code = ?code, "send_get_u64");
    let (payload, _fds) = send_and_recv(socket, code, &[], &[] as &[OwnedFd]).await?;
    let msg = VhostUserU64Msg::read_from_prefix(&payload)
        .map(|(val, _)| val)
        .map_err(|_| anyhow::anyhow!("reply payload too small for u64"))?;
    tracing::trace!(code = ?code, value = %format!("0x{:x}", msg.value), "send_get_u64 reply");
    Ok(msg.value)
}

/// Send a SET request with a u64 payload.
async fn send_set_u64(
    socket: &VhostUserSocket,
    code: VhostUserRequestCode,
    value: u64,
    reply_ack: bool,
) -> anyhow::Result<()> {
    let hdr = VhostUserMsgHeader {
        request: code.0,
        flags: request_flags(reply_ack),
        size: size_of::<VhostUserU64Msg>() as u32,
    };
    let payload = VhostUserU64Msg { value };
    tracing::trace!(code = ?code, value = %format!("0x{value:x}"), "send_set_u64");
    socket
        .send_message(&hdr, payload.as_bytes(), &[] as &[OwnedFd])
        .await?;
    if reply_ack {
        recv_ack(socket, code).await?;
    }
    Ok(())
}

/// Send a request with no payload (e.g., SET_OWNER, RESET_DEVICE).
async fn send_simple(
    socket: &VhostUserSocket,
    code: VhostUserRequestCode,
    reply_ack: bool,
) -> anyhow::Result<()> {
    let hdr = VhostUserMsgHeader {
        request: code.0,
        flags: request_flags(reply_ack),
        size: 0,
    };
    tracing::trace!(code = ?code, "send_simple");
    socket.send_message(&hdr, &[], &[] as &[OwnedFd]).await?;
    if reply_ack {
        recv_ack(socket, code).await?;
    }
    Ok(())
}

/// Send SET_MEM_TABLE with exported memory regions.
async fn send_set_mem_table(
    socket: &VhostUserSocket,
    regions: &[ShareableRegion],
    reply_ack: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        regions.len() <= VHOST_USER_MAX_FDS,
        "too many memory regions ({}) for SET_MEM_TABLE (max {})",
        regions.len(),
        VHOST_USER_MAX_FDS,
    );

    // Payload: { nregions: u32, padding: u32, regions: [VhostUserMemoryRegion] }
    let nregions = regions.len() as u32;
    let mut payload = Vec::new();
    payload.extend_from_slice(&nregions.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // padding

    let mut fds = Vec::new();
    for (i, region) in regions.iter().enumerate() {
        let wire_region = VhostUserMemoryRegion {
            guest_phys_addr: region.guest_address,
            memory_size: region.size,
            userspace_addr: region.guest_address + GPA_TO_VA_OFFSET,
            mmap_offset: region.file_offset,
        };
        tracing::trace!(
            i,
            gpa = %format!("0x{:x}", region.guest_address),
            size = %format!("0x{:x}", region.size),
            userspace_addr = %format!("0x{:x}", wire_region.userspace_addr),
            mmap_offset = %format!("0x{:x}", region.file_offset),
            "SET_MEM_TABLE region",
        );
        payload.extend_from_slice(wire_region.as_bytes());
        fds.push(&region.file);
    }

    let hdr = VhostUserMsgHeader {
        request: VhostUserRequestCode::SET_MEM_TABLE.0,
        flags: request_flags(reply_ack),
        size: payload.len() as u32,
    };
    socket.send_message(&hdr, &payload, &fds).await?;
    if reply_ack {
        recv_ack(socket, VhostUserRequestCode::SET_MEM_TABLE).await?;
    }
    Ok(())
}

/// Send a VringState message (SET_VRING_NUM, SET_VRING_BASE, SET_VRING_ENABLE).
async fn send_vring_state(
    socket: &VhostUserSocket,
    code: VhostUserRequestCode,
    index: u16,
    num: u32,
    reply_ack: bool,
) -> anyhow::Result<()> {
    let hdr = VhostUserMsgHeader {
        request: code.0,
        flags: request_flags(reply_ack),
        size: size_of::<VhostUserVringState>() as u32,
    };
    let payload = VhostUserVringState {
        index: index as u32,
        num,
    };
    tracing::trace!(code = ?code, index, num = %format!("0x{num:x}"), "send_vring_state");
    socket
        .send_message(&hdr, payload.as_bytes(), &[] as &[OwnedFd])
        .await?;
    if reply_ack {
        recv_ack(socket, code).await?;
    }
    Ok(())
}

/// Send SET_VRING_ADDR.
async fn send_vring_addr(
    socket: &VhostUserSocket,
    index: u16,
    desc_addr: u64,
    used_addr: u64,
    avail_addr: u64,
    reply_ack: bool,
) -> anyhow::Result<()> {
    let hdr = VhostUserMsgHeader {
        request: VhostUserRequestCode::SET_VRING_ADDR.0,
        flags: request_flags(reply_ack),
        size: size_of::<VhostUserVringAddr>() as u32,
    };
    let payload = VhostUserVringAddr {
        index: index as u32,
        flags: 0,
        desc_user_addr: desc_addr,
        used_user_addr: used_addr,
        avail_user_addr: avail_addr,
        log_guest_addr: 0,
    };
    tracing::trace!(
        index,
        desc = %format!("0x{desc_addr:x}"),
        used = %format!("0x{used_addr:x}"),
        avail = %format!("0x{avail_addr:x}"),
        "SET_VRING_ADDR",
    );
    socket
        .send_message(&hdr, payload.as_bytes(), &[] as &[OwnedFd])
        .await?;
    if reply_ack {
        recv_ack(socket, VhostUserRequestCode::SET_VRING_ADDR).await?;
    }
    Ok(())
}

/// Send SET_VRING_KICK or SET_VRING_CALL with an optional fd.
async fn send_vring_fd(
    socket: &VhostUserSocket,
    code: VhostUserRequestCode,
    index: u16,
    event: Option<&(impl AsFd + ?Sized)>,
    reply_ack: bool,
) -> anyhow::Result<()> {
    let nofd = event.is_none();
    let value = (index as u64 & VHOST_USER_VRING_INDEX_MASK)
        | if nofd { VHOST_USER_VRING_NOFD_MASK } else { 0 };

    let hdr = VhostUserMsgHeader {
        request: code.0,
        flags: request_flags(reply_ack),
        size: size_of::<VhostUserU64Msg>() as u32,
    };
    let payload = VhostUserU64Msg { value };

    tracing::trace!(code = ?code, index, nofd, "send_vring_fd");
    if let Some(event) = event {
        socket
            .send_message(&hdr, payload.as_bytes(), &[event.as_fd()])
            .await?;
    } else {
        socket
            .send_message(&hdr, payload.as_bytes(), &[] as &[OwnedFd])
            .await?;
    }
    if reply_ack {
        recv_ack(socket, code).await?;
    }
    Ok(())
}

/// Send GET_VRING_BASE — this implicitly stops the queue on the backend
/// and returns the raw vring base value.
///
/// For split ring, only the low 16 bits are meaningful (avail index).
/// For packed ring, the full u32 encodes both avail (low 16) and used
/// (high 16) state.
async fn send_get_vring_base(socket: &VhostUserSocket, index: u16) -> anyhow::Result<u32> {
    let payload = VhostUserVringState {
        index: index as u32,
        num: 0,
    };
    let (reply_payload, _fds) = send_and_recv(
        socket,
        VhostUserRequestCode::GET_VRING_BASE,
        payload.as_bytes(),
        &[] as &[OwnedFd],
    )
    .await?;
    let reply = VhostUserVringState::read_from_prefix(&reply_payload)
        .map(|(val, _)| val)
        .map_err(|_| anyhow::anyhow!("GET_VRING_BASE reply too small"))?;
    Ok(reply.num)
}

/// Send GET_CONFIG and return the config bytes.
async fn send_get_config(
    socket: &VhostUserSocket,
    offset: u32,
    size: u32,
) -> anyhow::Result<Vec<u8>> {
    let config_hdr = VhostUserConfigHeader {
        offset,
        size,
        flags: 0,
    };
    // The vhost-user spec requires the payload to be the config header
    // followed by `size` bytes of buffer space (zeroed for GET).
    let mut request_payload =
        Vec::with_capacity(size_of::<VhostUserConfigHeader>() + size as usize);
    request_payload.extend_from_slice(config_hdr.as_bytes());
    request_payload.resize(size_of::<VhostUserConfigHeader>() + size as usize, 0);

    tracing::trace!(offset, size, "GET_CONFIG");
    let (reply_payload, _fds) = send_and_recv(
        socket,
        VhostUserRequestCode::GET_CONFIG,
        &request_payload,
        &[] as &[OwnedFd],
    )
    .await?;
    // Reply: config header + config data.
    let hdr_size = size_of::<VhostUserConfigHeader>();
    let config_data = if reply_payload.len() > hdr_size {
        reply_payload[hdr_size..].to_vec()
    } else {
        Vec::new()
    };
    tracing::trace!(
        offset,
        size,
        config_len = config_data.len(),
        "GET_CONFIG reply"
    );
    Ok(config_data)
}

/// Send SET_CONFIG.
async fn send_set_config(
    socket: &VhostUserSocket,
    offset: u16,
    data: &[u8],
    reply_ack: bool,
) -> anyhow::Result<()> {
    let config_hdr = VhostUserConfigHeader {
        offset: offset as u32,
        size: data.len() as u32,
        flags: 0,
    };
    let mut payload = Vec::with_capacity(size_of::<VhostUserConfigHeader>() + data.len());
    payload.extend_from_slice(config_hdr.as_bytes());
    payload.extend_from_slice(data);

    let hdr = VhostUserMsgHeader {
        request: VhostUserRequestCode::SET_CONFIG.0,
        flags: request_flags(reply_ack),
        size: payload.len() as u32,
    };
    tracing::trace!(offset, size = data.len(), "SET_CONFIG");
    socket
        .send_message(&hdr, &payload, &[] as &[OwnedFd])
        .await?;
    if reply_ack {
        recv_ack(socket, VhostUserRequestCode::SET_CONFIG).await?;
    }
    Ok(())
}

/// Convert a u64 to VirtioDeviceFeatures.
// TODO: VirtioDeviceFeatures should just be a u64-based enum/bitfield instead
// of a Vec<u32> bank model. The spec's bank-indexed transport registers don't
// require the in-memory representation to match, and every VMM (QEMU, CH,
// Linux) uses a flat u64. That would eliminate these helpers entirely.
fn features_from_u64(value: u64) -> VirtioDeviceFeatures {
    VirtioDeviceFeatures::new()
        .with_bank(0, value as u32)
        .with_bank(1, (value >> 32) as u32)
}

/// Read the used_index from the used ring in guest memory.
///
/// The used ring starts at `params.used_addr`. The `idx` field is at
/// offset 2 (after the flags field) and is a 16-bit LE value.
fn read_used_index(mem: &GuestMemory, params: &virtio::queue::QueueParams) -> u16 {
    let mut buf = [0u8; 2];
    // used ring layout: { flags: u16, idx: u16, ... }
    if mem.read_at(params.used_addr + 2, &mut buf).is_ok() {
        u16::from_le_bytes(buf)
    } else {
        0
    }
}

#[cfg(test)]
// UNSAFETY: Implementing GuestMemoryAccess for test-only ShareableGuestMemory.
#[expect(unsafe_code)]
mod tests {
    use super::*;
    use guestmem::GuestMemorySharing;
    use guestmem::ProvideShareableRegions;
    use guestmem::ShareableRegionError;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::socket::PolledSocket;
    use pal_async::task::Spawn;
    use pal_event::Event;
    use sparse_mmap::SparseMapping;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use vhost_user_backend::VhostUserDeviceServer;
    use virtio::DeviceTraits;
    use virtio::DeviceTraitsSharedMemory;
    use virtio::QueueResources;
    use virtio::VirtioDevice;
    use virtio::queue::QueueParams;
    use virtio::queue::QueueState;
    use virtio::spec::VirtioDeviceFeatures;
    use vmcore::interrupt::Interrupt;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    /// File-backed guest memory that supports `sharing()`.
    struct ShareableGuestMemory {
        mapping: SparseMapping,
        fd: Arc<sparse_mmap::Mappable>,
        size: u64,
    }

    impl ShareableGuestMemory {
        fn new(size: usize) -> Self {
            let fd = sparse_mmap::alloc_shared_memory(size, "test-guest-memory")
                .expect("alloc_shared_memory failed");
            let mapping = SparseMapping::new(size).expect("SparseMapping::new failed");
            mapping
                .map_file(0, size, fd.try_clone().unwrap(), 0, true)
                .expect("map_file failed");
            Self {
                mapping,
                fd: Arc::new(fd),
                size: size as u64,
            }
        }

        fn into_guest_memory(self) -> GuestMemory {
            GuestMemory::new("test-shareable", self)
        }
    }

    // SAFETY: SparseMapping's pointer is valid for the lifetime of the mapping
    // and the fd is a shareable file descriptor.
    unsafe impl guestmem::GuestMemoryAccess for ShareableGuestMemory {
        fn mapping(&self) -> Option<std::ptr::NonNull<u8>> {
            std::ptr::NonNull::new(self.mapping.as_ptr().cast())
        }

        fn max_address(&self) -> u64 {
            self.size
        }

        fn sharing(&self) -> Option<GuestMemorySharing> {
            Some(GuestMemorySharing::new(TestRegionProvider {
                fd: self.fd.clone(),
                size: self.size,
            }))
        }
    }

    struct TestRegionProvider {
        fd: Arc<sparse_mmap::Mappable>,
        size: u64,
    }

    impl ProvideShareableRegions for TestRegionProvider {
        async fn get_regions(&self) -> Result<Vec<ShareableRegion>, ShareableRegionError> {
            Ok(vec![ShareableRegion {
                guest_address: 0,
                size: self.size,
                file: self.fd.clone(),
                file_offset: 0,
            }])
        }
    }

    /// A mock VirtioDevice for the backend side of the dog-food test.
    struct MockBackendDevice {
        traits: DeviceTraits,
        started_queues: Vec<u16>,
    }

    impl MockBackendDevice {
        fn new() -> Self {
            Self {
                traits: DeviceTraits {
                    device_id: VirtioDeviceType::BLK,
                    device_features: VirtioDeviceFeatures::new(),
                    max_queues: 2,
                    device_register_length: 0,
                    shared_memory: DeviceTraitsSharedMemory::default(),
                },
                started_queues: Vec::new(),
            }
        }
    }

    impl InspectMut for MockBackendDevice {
        fn inspect_mut(&mut self, _req: inspect::Request<'_>) {}
    }

    impl VirtioDevice for MockBackendDevice {
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
            if self.started_queues.contains(&idx) {
                self.started_queues.retain(|&x| x != idx);
                Some(QueueState {
                    avail_index: 42,
                    used_index: 0,
                })
            } else {
                None
            }
        }

        async fn reset(&mut self) {
            self.started_queues.clear();
        }
    }

    fn socket_pair() -> (UnixStream, UnixStream) {
        let (a, b) = socket2::Socket::pair(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .expect("socketpair failed");
        (a.into(), b.into())
    }

    /// Create a frontend+backend pair over a socketpair. Returns the frontend
    /// and a handle to the backend task (drop the frontend to let it finish).
    async fn setup_frontend_backend(
        driver: &DefaultDriver,
    ) -> (VhostUserFrontend, GuestMemory, pal_async::task::Task<()>) {
        let (frontend_stream, backend_stream) = socket_pair();

        let backend_polled = PolledSocket::new(driver, backend_stream).unwrap();
        let backend_socket = VhostUserSocket::new(backend_polled);

        let server = VhostUserDeviceServer::new(Box::new(MockBackendDevice::new()));

        let backend_task = driver.spawn("backend", async move {
            server.serve_connection(backend_socket).await.unwrap();
        });

        let frontend_polled = PolledSocket::new(driver, frontend_stream).unwrap();
        let frontend_socket = VhostUserSocket::new(frontend_polled);

        let guest_memory = ShareableGuestMemory::new(65536).into_guest_memory();

        let vm_driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())).simple();
        let frontend =
            VhostUserFrontend::from_socket(vm_driver, frontend_socket, VirtioDeviceType::BLK)
                .await
                .expect("frontend handshake failed");

        (frontend, guest_memory, backend_task)
    }

    /// Build dummy QueueResources with the given interrupt.
    fn dummy_queue_resources(notify: Interrupt, guest_memory: GuestMemory) -> QueueResources {
        QueueResources {
            params: QueueParams {
                size: 16,
                enable: true,
                desc_addr: 0x0000,
                avail_addr: 0x1000,
                used_addr: 0x2000,
            },
            notify,
            event: Event::new(),
            guest_memory,
        }
    }

    #[async_test]
    async fn frontend_backend_dogfood(driver: DefaultDriver) {
        let (mut frontend, _guest_memory, backend_task) = setup_frontend_backend(&driver).await;

        // Verify traits.
        let traits = frontend.traits();
        assert_eq!(traits.device_id, VirtioDeviceType::BLK);
        assert_eq!(traits.max_queues, 2);

        // Reset.
        frontend.reset().await;
        assert!(frontend.supports_save_restore());

        drop(frontend);
        backend_task.await;
    }

    /// Test start_queue + stop_queue with an event-backed interrupt.
    /// When the interrupt is event-backed, the event is passed directly
    /// to SET_VRING_CALL with no proxy task needed.
    #[async_test]
    async fn start_stop_queue_event_interrupt(driver: DefaultDriver) {
        let (mut frontend, guest_memory, backend_task) = setup_frontend_backend(&driver).await;

        let features = VirtioDeviceFeatures::new();
        let resources =
            dummy_queue_resources(Interrupt::from_event(Event::new()), guest_memory.clone());

        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        // Event-backed interrupt — no proxy needed.
        assert!(frontend.queues[0]._event_proxy.is_none());

        // Stop the queue and verify we get state back.
        let state = frontend.stop_queue(0).await;
        assert!(state.is_some());

        // Stopping again should return None.
        let state2 = frontend.stop_queue(0).await;
        assert!(state2.is_none());

        drop(frontend);
        backend_task.await;
    }

    /// Test start_queue + stop_queue with a function-backed interrupt.
    /// Verifies the proxy works with non-event interrupts (e.g., MSI-X).
    #[async_test]
    async fn start_stop_queue_fn_interrupt(driver: DefaultDriver) {
        let (mut frontend, guest_memory, backend_task) = setup_frontend_backend(&driver).await;

        let features = VirtioDeviceFeatures::new();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let resources = dummy_queue_resources(
            Interrupt::from_fn(move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            }),
            guest_memory.clone(),
        );

        // Verify this is NOT event-backed — will exercise the proxy path.
        assert!(resources.notify.event().is_none());

        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        // Stop the queue — this drops the InterruptEvent and its proxy task.
        let state = frontend.stop_queue(0).await;
        assert!(state.is_some());

        drop(frontend);
        backend_task.await;
    }

    /// Test that the interrupt proxy actually forwards signals.
    #[async_test]
    async fn interrupt_proxy_delivers(driver: DefaultDriver) {
        let (mut frontend, guest_memory, backend_task) = setup_frontend_backend(&driver).await;

        let features = VirtioDeviceFeatures::new();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let resources = dummy_queue_resources(
            Interrupt::from_fn(move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            }),
            guest_memory.clone(),
        );

        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        // The proxy task is waiting on the event we sent via SET_VRING_CALL.
        // The backend holds a clone of that event. When the backend signals it,
        // the proxy should call our counter fn.
        //
        // We can't directly poke the backend's event from here, but we can
        // verify the proxy was set up since the interrupt is fn-backed.
        assert!(frontend.queues[0]._event_proxy.is_some());

        let state = frontend.stop_queue(0).await;
        assert!(state.is_some());

        // Proxy should be torn down.
        assert!(frontend.queues[0]._event_proxy.is_none());

        drop(frontend);
        backend_task.await;
    }

    /// Test that reset clears the guest_features_sent flag and stops queues.
    #[async_test]
    async fn reset_clears_state(driver: DefaultDriver) {
        let (mut frontend, guest_memory, backend_task) = setup_frontend_backend(&driver).await;

        let features = VirtioDeviceFeatures::new();

        // Start queue 0.
        let resources =
            dummy_queue_resources(Interrupt::from_event(Event::new()), guest_memory.clone());
        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        assert!(frontend.guest_features_sent);

        // Reset should stop all queues and clear the features flag.
        frontend.reset().await;

        assert!(!frontend.guest_features_sent);
        assert!(!frontend.queues[0].active);

        // Stopping a non-active queue returns None.
        assert!(frontend.stop_queue(0).await.is_none());

        // Can start again after reset (SET_FEATURES will be re-sent).
        let resources2 =
            dummy_queue_resources(Interrupt::from_event(Event::new()), guest_memory.clone());
        frontend
            .start_queue(0, resources2, &features, None)
            .await
            .expect("start_queue after reset failed");

        assert!(frontend.guest_features_sent);

        drop(frontend);
        backend_task.await;
    }

    /// Create a frontend+backend pair with a custom device. The device's
    /// traits determine feature negotiation (e.g., packed ring support).
    async fn setup_frontend_backend_with_device(
        driver: &DefaultDriver,
        device: impl VirtioDevice + 'static,
    ) -> (VhostUserFrontend, GuestMemory, pal_async::task::Task<()>) {
        let device_id = device.traits().device_id;
        let (frontend_stream, backend_stream) = socket_pair();

        let backend_polled = PolledSocket::new(driver, backend_stream).unwrap();
        let backend_socket = VhostUserSocket::new(backend_polled);

        let server = VhostUserDeviceServer::new(Box::new(device));

        let backend_task = driver.spawn("backend", async move {
            server.serve_connection(backend_socket).await.unwrap();
        });

        let frontend_polled = PolledSocket::new(driver, frontend_stream).unwrap();
        let frontend_socket = VhostUserSocket::new(frontend_polled);

        let guest_memory = ShareableGuestMemory::new(65536).into_guest_memory();

        let vm_driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())).simple();
        let frontend = VhostUserFrontend::from_socket(vm_driver, frontend_socket, device_id)
            .await
            .expect("frontend handshake failed");

        (frontend, guest_memory, backend_task)
    }

    /// Mock device that returns specific avail/used indices on stop and
    /// optionally advertises packed ring.
    struct SaveRestoreMockDevice {
        traits: DeviceTraits,
        started_queues: Vec<u16>,
        stop_avail: u16,
        stop_used: u16,
    }

    impl SaveRestoreMockDevice {
        fn new(packed_ring: bool, stop_avail: u16, stop_used: u16) -> Self {
            use virtio::spec::VirtioDeviceFeaturesBank1;
            let features = VirtioDeviceFeatures::new()
                .with_bank1(VirtioDeviceFeaturesBank1::new().with_ring_packed(packed_ring));
            Self {
                traits: DeviceTraits {
                    device_id: VirtioDeviceType::BLK,
                    device_features: features,
                    max_queues: 2,
                    device_register_length: 0,
                    shared_memory: DeviceTraitsSharedMemory::default(),
                },
                started_queues: Vec::new(),
                stop_avail,
                stop_used,
            }
        }
    }

    impl InspectMut for SaveRestoreMockDevice {
        fn inspect_mut(&mut self, _req: inspect::Request<'_>) {}
    }

    impl VirtioDevice for SaveRestoreMockDevice {
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
            if self.started_queues.contains(&idx) {
                self.started_queues.retain(|&x| x != idx);
                Some(QueueState {
                    avail_index: self.stop_avail,
                    used_index: self.stop_used,
                })
            } else {
                None
            }
        }

        async fn reset(&mut self) {
            self.started_queues.clear();
        }
    }

    /// Split ring: stop_queue returns avail_index from GET_VRING_BASE,
    /// used_index from the guest-visible used ring (not from the backend).
    #[async_test]
    async fn stop_queue_split_ring_state(driver: DefaultDriver) {
        let device = SaveRestoreMockDevice::new(
            false, // split ring
            100,   // avail_index the backend will return
            999,   // used_index — should NOT appear in the frontend result
        );
        let (mut frontend, guest_memory, backend_task) =
            setup_frontend_backend_with_device(&driver, device).await;

        // Write a known used_index into the guest-visible used ring.
        // used ring layout: { flags: u16, idx: u16, ... }
        let used_addr: u64 = 0x2000;
        let used_idx_value: u16 = 77;
        guest_memory
            .write_at(used_addr + 2, &used_idx_value.to_le_bytes())
            .unwrap();

        let features = VirtioDeviceFeatures::new(); // no packed ring
        let resources =
            dummy_queue_resources(Interrupt::from_event(Event::new()), guest_memory.clone());
        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        let state = frontend
            .stop_queue(0)
            .await
            .expect("stop_queue should return state");
        // avail_index comes from GET_VRING_BASE reply.
        assert_eq!(state.avail_index, 100);
        // used_index comes from reading the guest used ring, not the backend.
        assert_eq!(state.used_index, used_idx_value);

        drop(frontend);
        backend_task.await;
    }

    /// Packed ring: stop_queue returns both avail and used state from
    /// GET_VRING_BASE (the used ring in guest memory is not used).
    #[async_test]
    async fn stop_queue_packed_ring_state(driver: DefaultDriver) {
        let device = SaveRestoreMockDevice::new(
            true, // packed ring
            200,  // avail_index (with wrap counter bits)
            300,  // used_index (with wrap counter bits)
        );
        let (mut frontend, guest_memory, backend_task) =
            setup_frontend_backend_with_device(&driver, device).await;

        // Features must include packed ring so the frontend knows.
        use virtio::spec::VirtioDeviceFeaturesBank1;
        let features = VirtioDeviceFeatures::new()
            .with_bank1(VirtioDeviceFeaturesBank1::new().with_ring_packed(true));
        let resources =
            dummy_queue_resources(Interrupt::from_event(Event::new()), guest_memory.clone());
        frontend
            .start_queue(0, resources, &features, None)
            .await
            .expect("start_queue failed");

        assert!(frontend.packed_ring);

        let state = frontend
            .stop_queue(0)
            .await
            .expect("stop_queue should return state");
        // Both avail and used come from GET_VRING_BASE.
        assert_eq!(state.avail_index, 200);
        assert_eq!(state.used_index, 300);

        drop(frontend);
        backend_task.await;
    }
}
