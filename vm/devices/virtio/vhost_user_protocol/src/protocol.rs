// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vhost-user wire protocol types.
//!
//! Reference: <https://qemu-project.gitlab.io/qemu/interop/vhost-user.html>

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Maximum number of file descriptors that can be sent in a single message.
pub const VHOST_USER_MAX_FDS: usize = 8;

/// Header flag: protocol version (always 1).
pub const VHOST_USER_FLAG_VERSION: u32 = 0x1;
/// Header flag: this message is a reply.
pub const VHOST_USER_FLAG_REPLY: u32 = 0x4;
/// Header flag: the frontend requests a reply to this message.
pub const VHOST_USER_FLAG_NEED_REPLY: u32 = 0x8;

/// SET_VRING_KICK/CALL/ERR: bit 8 means "no fd".
pub const VHOST_USER_VRING_NOFD_MASK: u64 = 1 << 8;
/// Index bits for SET_VRING_KICK/CALL/ERR.
pub const VHOST_USER_VRING_INDEX_MASK: u64 = 0xFF;

open_enum! {
    /// vhost-user frontend-to-backend request types.
    pub enum VhostUserRequestCode: u32 {
        NONE = 0,
        GET_FEATURES = 1,
        SET_FEATURES = 2,
        SET_OWNER = 3,
        RESET_OWNER = 4,
        SET_MEM_TABLE = 5,
        SET_LOG_BASE = 6,
        SET_LOG_FD = 7,
        SET_VRING_NUM = 8,
        SET_VRING_ADDR = 9,
        SET_VRING_BASE = 10,
        GET_VRING_BASE = 11,
        SET_VRING_KICK = 12,
        SET_VRING_CALL = 13,
        SET_VRING_ERR = 14,
        GET_PROTOCOL_FEATURES = 15,
        SET_PROTOCOL_FEATURES = 16,
        GET_QUEUE_NUM = 17,
        SET_VRING_ENABLE = 18,
        SEND_RARP = 19,
        NET_SET_MTU = 20,
        SET_BACKEND_REQ_FD = 21,
        IOTLB_MSG = 22,
        SET_VRING_ENDIAN = 23,
        GET_CONFIG = 24,
        SET_CONFIG = 25,
        CRYPTO_CREATE_SESS = 26,
        CRYPTO_CLOSE_SESS = 27,
        POSTCOPY_ADVISE = 28,
        POSTCOPY_LISTEN = 29,
        POSTCOPY_END = 30,
        GET_INFLIGHT_FD = 31,
        SET_INFLIGHT_FD = 32,
        GPU_SET_SOCKET = 33,
        RESET_DEVICE = 34,
        VRING_KICK = 35,
        GET_MAX_MEM_SLOTS = 36,
        ADD_MEM_REGION = 37,
        REM_MEM_REGION = 38,
        SET_STATUS = 39,
        GET_STATUS = 40,
        GET_SHARED_OBJECT = 41,
        SET_DEVICE_STATE_FD = 42,
        CHECK_DEVICE_STATE = 43,
    }
}

/// vhost-user message header (12 bytes on the wire).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserMsgHeader {
    pub request: u32,
    pub flags: u32,
    pub size: u32,
}

impl VhostUserMsgHeader {
    /// Create a reply header for the given request header.
    pub fn reply(request: &Self, payload_size: u32) -> Self {
        Self {
            request: request.request,
            flags: VHOST_USER_FLAG_VERSION | VHOST_USER_FLAG_REPLY,
            size: payload_size,
        }
    }

    /// The request code.
    pub fn code(&self) -> VhostUserRequestCode {
        VhostUserRequestCode(self.request)
    }

    /// Whether this is a reply message.
    pub fn is_reply(&self) -> bool {
        self.flags & VHOST_USER_FLAG_REPLY != 0
    }

    /// Whether the NEED_REPLY flag is set.
    pub fn need_reply(&self) -> bool {
        self.flags & VHOST_USER_FLAG_NEED_REPLY != 0
    }

    /// Whether the version field is valid.
    pub fn version_valid(&self) -> bool {
        self.flags & VHOST_USER_FLAG_VERSION != 0
    }
}

/// Fixed header for SET_MEM_TABLE payload: region count + padding.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserMemoryHeader {
    pub nregions: u32,
    pub padding: u32,
}

/// Payload for SET_MEM_TABLE — variable-length, preceded by region count.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserMemoryRegion {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    pub mmap_offset: u64,
}

/// Payload for SET_VRING_ADDR.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserVringAddr {
    pub index: u32,
    pub flags: u32,
    pub desc_user_addr: u64,
    pub used_user_addr: u64,
    pub avail_user_addr: u64,
    pub log_guest_addr: u64,
}

/// Payload for SET_VRING_NUM, SET_VRING_BASE, GET_VRING_BASE, SET_VRING_ENABLE.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserVringState {
    pub index: u32,
    pub num: u32,
}

/// Payload for GET_CONFIG / SET_CONFIG.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserConfigHeader {
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

/// Payload for messages carrying a single u64 value (SET_FEATURES, etc.)
/// and for SET_VRING_KICK/CALL/ERR (fd via SCM_RIGHTS, index in low bits).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhostUserU64Msg {
    pub value: u64,
}

/// Protocol feature flags negotiated via GET/SET_PROTOCOL_FEATURES.
#[bitfield(u64)]
pub struct VhostUserProtocolFeatures {
    pub mq: bool,
    pub log_shmfd: bool,
    pub rarp: bool,
    pub reply_ack: bool,
    pub mtu: bool,
    pub backend_req: bool,
    pub cross_endian: bool,
    pub crypto_session: bool,
    pub pagefault: bool,
    pub config: bool,
    pub backend_send_fd: bool,
    pub host_notifier: bool,
    pub inflight_shmfd: bool,
    pub reset_device: bool,
    pub inband_notifications: bool,
    pub configure_mem_slots: bool,
    pub status: bool,
    pub xen_mmap: bool,
    pub shared_object: bool,
    pub device_state: bool,
    pub get_vring_base_inflight: bool,
    #[bits(43)]
    _reserved: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;

    #[test]
    fn header_size() {
        assert_eq!(size_of::<VhostUserMsgHeader>(), 12);
    }

    #[test]
    fn region_size() {
        assert_eq!(size_of::<VhostUserMemoryRegion>(), 32);
    }

    #[test]
    fn vring_addr_size() {
        assert_eq!(size_of::<VhostUserVringAddr>(), 40);
    }

    #[test]
    fn vring_state_size() {
        assert_eq!(size_of::<VhostUserVringState>(), 8);
    }

    #[test]
    fn config_header_size() {
        assert_eq!(size_of::<VhostUserConfigHeader>(), 12);
    }

    #[test]
    fn u64_msg_size() {
        assert_eq!(size_of::<VhostUserU64Msg>(), 8);
    }

    #[test]
    fn header_roundtrip() {
        let hdr = VhostUserMsgHeader {
            request: VhostUserRequestCode::GET_FEATURES.0,
            flags: VHOST_USER_FLAG_VERSION,
            size: 8,
        };
        let bytes = hdr.as_bytes();
        let decoded = VhostUserMsgHeader::read_from_bytes(bytes).unwrap();
        assert_eq!(decoded.request, hdr.request);
        assert_eq!(decoded.flags, hdr.flags);
        assert_eq!(decoded.size, hdr.size);
    }

    #[test]
    fn memory_region_roundtrip() {
        let region = VhostUserMemoryRegion {
            guest_phys_addr: 0x1000,
            memory_size: 0x2000,
            userspace_addr: 0x7f00_0000,
            mmap_offset: 0,
        };
        let bytes = region.as_bytes();
        let decoded = VhostUserMemoryRegion::read_from_bytes(bytes).unwrap();
        assert_eq!(decoded.guest_phys_addr, region.guest_phys_addr);
        assert_eq!(decoded.memory_size, region.memory_size);
    }

    #[test]
    fn protocol_features() {
        let pf = VhostUserProtocolFeatures::new()
            .with_mq(true)
            .with_config(true);
        assert!(pf.mq());
        assert!(pf.config());
        assert!(!pf.rarp());
    }

    #[test]
    fn request_code_unknown() {
        let code = VhostUserRequestCode(9999);
        // Should not panic — open_enum handles unknown values.
        let _ = format!("{:?}", code);
    }
}
