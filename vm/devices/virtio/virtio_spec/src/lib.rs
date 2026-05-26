// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Types and constants defined by the virtio specification.
//!
//! Reference: <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html>

#![expect(missing_docs)]

pub mod blk;
pub mod fs;

use bitfield_struct::bitfield;
use inspect::Inspect;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub use packed_nums::*;

#[expect(non_camel_case_types)]
mod packed_nums {
    pub type u16_le = zerocopy::U16<zerocopy::LittleEndian>;
    pub type u32_le = zerocopy::U32<zerocopy::LittleEndian>;
    pub type u64_le = zerocopy::U64<zerocopy::LittleEndian>;
}

#[derive(Inspect)]
#[bitfield(u64)]
pub struct VirtioDeviceFeatures {
    // Bank 0 (bits 0-31)
    #[bits(24)]
    pub device_specific_low: u32,
    #[bits(4)]
    _reserved1: u8,
    pub ring_indirect_desc: bool, // VIRTIO_F_INDIRECT_DESC (bit 28)
    pub ring_event_idx: bool,     // VIRTIO_F_EVENT_IDX (bit 29)
    pub vhost_user_protocol_features: bool, // VHOST_USER_F_PROTOCOL_FEATURES (bit 30)
    _reserved2: bool,
    // Bank 1 (bits 32-63)
    pub version_1: bool,         // VIRTIO_F_VERSION_1 (bit 32)
    pub access_platform: bool,   // VIRTIO_F_ACCESS_PLATFORM (bit 33)
    pub ring_packed: bool,       // VIRTIO_F_RING_PACKED (bit 34)
    pub in_order: bool,          // VIRTIO_F_IN_ORDER (bit 35)
    pub order_platform: bool,    // VIRTIO_F_ORDER_PLATFORM (bit 36)
    pub sriov: bool,             // VIRTIO_F_SR_IOV (bit 37)
    pub notification_data: bool, // VIRTIO_F_NOTIFICATION_DATA (bit 38)
    pub notif_config_data: bool, // VIRTIO_F_NOTIF_CONFIG_DATA (bit 39)
    pub ring_reset: bool,        // VIRTIO_F_RING_RESET (bit 40)
    pub admin_vq: bool,          // VIRTIO_F_ADMIN_VQ (bit 41)
    pub device_specific_bit_42: bool,
    pub suspend: bool, // VIRTIO_F_SUSPEND (bit 43)
    #[bits(7)]
    _reserved3: u8,
    #[bits(13)]
    pub device_specific_high: u16,
}

impl VirtioDeviceFeatures {
    /// Get a 32-bit bank by index (0 = low, 1 = high) for transport register
    /// reads. Returns 0 for out-of-range indices.
    pub fn bank(&self, index: usize) -> u32 {
        match index {
            0 => self.into_bits() as u32,
            1 => (self.into_bits() >> 32) as u32,
            _ => 0,
        }
    }

    /// Set a 32-bit bank by index for transport register writes.
    pub fn set_bank(&mut self, index: usize, val: u32) {
        let bits = self.into_bits();
        *self = Self::from_bits(match index {
            0 => (bits & !0xFFFF_FFFF) | val as u64,
            1 => (bits & 0xFFFF_FFFF) | (val as u64) << 32,
            _ => return,
        });
    }

    /// Builder method to set a 32-bit bank by index.
    pub fn with_bank(mut self, index: usize, val: u32) -> Self {
        self.set_bank(index, val);
        self
    }
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioDeviceStatus {
    pub acknowledge: bool,
    pub driver: bool,
    pub driver_ok: bool,
    pub features_ok: bool,
    pub suspend: bool,
    _reserved1: bool,
    pub device_needs_reset: bool,
    pub failed: bool,
}

impl VirtioDeviceStatus {
    pub fn as_u32(&self) -> u32 {
        self.into_bits() as u32
    }
}

open_enum::open_enum! {
    /// Virtio device type IDs as defined by the virtio specification.
    ///
    /// Reference: <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html> §5
    pub enum VirtioDeviceType: u16 {
        NET = 1,
        BLK = 2,
        CONSOLE = 3,
        RNG = 4,
        P9 = 9,
        VSOCK = 19,
        FS = 26,
        PMEM = 27,
    }
}

// ACPI interrupt status flags
pub const VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER: u32 = 1;
pub const VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE: u32 = 2;

/// Virtio over PCI specific constants
pub mod pci {
    use open_enum::open_enum;

    open_enum! {
        /// Virtio PCI capability config type.
        pub enum VirtioPciCapType: u8 {
            COMMON_CFG = 1,
            NOTIFY_CFG = 2,
            ISR_CFG = 3,
            DEVICE_CFG = 4,
            // PCI_CFG = 5,
            SHARED_MEMORY_CFG = 8,
        }
    }

    pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
    pub const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040;

    open_enum! {
        /// Byte offsets within the `virtio_pci_common_cfg` structure,
        /// accessed at u32-aligned boundaries.
        pub enum VirtioPciCommonCfg: u16 {
            DEVICE_FEATURE_SELECT = 0,
            DEVICE_FEATURE = 4,
            DRIVER_FEATURE_SELECT = 8,
            DRIVER_FEATURE = 12,
            MSIX_CONFIG = 16,
            DEVICE_STATUS = 20,
            QUEUE_SIZE = 24,
            QUEUE_ENABLE = 28,
            QUEUE_DESC_LO = 32,
            QUEUE_DESC_HI = 36,
            QUEUE_AVAIL_LO = 40,
            QUEUE_AVAIL_HI = 44,
            QUEUE_USED_LO = 48,
            QUEUE_USED_HI = 52,
        }
    }

    /// Total size of the common configuration structure.
    pub const VIRTIO_PCI_COMMON_CFG_SIZE: u16 = 56;
}

/// Virtio over MMIO register offsets (virtio spec section 4.2.2)
pub mod mmio {
    use open_enum::open_enum;

    open_enum! {
        /// MMIO register offsets for virtio MMIO transport.
        pub enum VirtioMmioRegister: u16 {
            MAGIC_VALUE = 0x000,
            VERSION = 0x004,
            DEVICE_ID = 0x008,
            VENDOR_ID = 0x00c,
            DEVICE_FEATURES = 0x010,
            DEVICE_FEATURES_SEL = 0x014,
            DRIVER_FEATURES = 0x020,
            DRIVER_FEATURES_SEL = 0x024,
            QUEUE_SEL = 0x030,
            QUEUE_NUM_MAX = 0x034,
            QUEUE_NUM = 0x038,
            QUEUE_READY = 0x044,
            QUEUE_NOTIFY = 0x050,
            INTERRUPT_STATUS = 0x060,
            INTERRUPT_ACK = 0x064,
            STATUS = 0x070,
            QUEUE_DESC_LOW = 0x080,
            QUEUE_DESC_HIGH = 0x084,
            QUEUE_AVAIL_LOW = 0x090,
            QUEUE_AVAIL_HIGH = 0x094,
            QUEUE_USED_LOW = 0x0a0,
            QUEUE_USED_HIGH = 0x0a4,
            CONFIG_GENERATION = 0x0fc,
            CONFIG = 0x100,
        }
    }
}

/// Virtio queue definitions.
pub mod queue {
    use super::u16_le;
    use super::u32_le;
    use super::u64_le;
    use bitfield_struct::bitfield;
    use inspect::Inspect;

    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SplitDescriptor {
        pub address: u64_le,
        pub length: u32_le,
        pub flags_raw: u16_le,
        pub next: u16_le,
    }

    impl SplitDescriptor {
        pub fn flags(&self) -> DescriptorFlags {
            self.flags_raw.get().into()
        }
    }

    #[bitfield(u16)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct DescriptorFlags {
        pub next: bool,
        pub write: bool,
        pub indirect: bool,
        #[bits(4)]
        _reserved: u16,
        pub available: bool,
        #[bits(7)]
        _reserved2: u16,
        pub used: bool,
    }

    /*
    struct virtq_avail {
        le16 flags;
        le16 idx;
        le16 ring[ /* Queue Size */ ];
        le16 used_event;
    }
    */
    pub const AVAIL_OFFSET_FLAGS: u64 = 0;
    pub const AVAIL_OFFSET_IDX: u64 = 2;
    pub const AVAIL_OFFSET_RING: u64 = 4;
    pub const AVAIL_ELEMENT_SIZE: u64 = size_of::<u16>() as u64;

    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct AvailableFlags {
        pub no_interrupt: bool,
        #[bits(15)]
        _reserved: u16,
    }

    /*
    struct virtq_used {
        le16 flags;
        le16 idx;
        struct virtq_used_elem ring[ /* Queue Size */];
        le16 avail_event;
    };
    */
    pub const USED_OFFSET_FLAGS: u64 = 0;
    pub const USED_OFFSET_IDX: u64 = 2;
    pub const USED_OFFSET_RING: u64 = 4;
    pub const USED_ELEMENT_SIZE: u64 = size_of::<UsedElement>() as u64;

    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct UsedElement {
        pub id: u32_le,
        pub len: u32_le,
    }

    #[bitfield(u16)]
    pub struct UsedFlags {
        pub no_notify: bool,
        #[bits(15)]
        _reserved: u16,
    }

    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PackedDescriptor {
        pub address: u64_le,
        pub length: u32_le,
        pub buffer_id: u16_le,
        pub flags_raw: u16_le,
    }

    impl PackedDescriptor {
        pub fn new() -> Self {
            Self {
                address: u64_le::new(0),
                length: u32_le::new(0),
                buffer_id: u16_le::new(0),
                flags_raw: u16_le::new(0),
            }
        }

        pub fn with_buffer_id(mut self, buffer_id: u16) -> Self {
            self.buffer_id = u16_le::new(buffer_id);
            self
        }

        pub fn with_length(mut self, length: u32) -> Self {
            self.length = u32_le::new(length);
            self
        }

        pub fn with_flags(mut self, flags: DescriptorFlags) -> Self {
            self.flags_raw = u16_le::new(flags.into_bits());
            self
        }

        pub fn flags(&self) -> DescriptorFlags {
            self.flags_raw.get().into()
        }
    }

    /// Flags controlling event (interrupt/notification) suppression for packed
    /// virtqueues.
    ///
    /// Reference: virtio spec §2.8.10, "Event Suppression Structure Layout".
    #[derive(Debug, PartialEq, Eq, Inspect)]
    #[repr(u8)]
    pub enum EventSuppressionFlags {
        /// `RING_EVENT_FLAGS_ENABLE` (0x0) — events are enabled; the device/driver
        /// should generate events (interrupts or notifications) normally.
        Enabled = 0,
        /// `RING_EVENT_FLAGS_DISABLE` (0x1) — events are disabled; the
        /// device/driver should not generate any events.
        Disabled = 1,
        /// `RING_EVENT_FLAGS_DESC` (0x2) — enable events only when a specific
        /// descriptor index (with matching wrap counter) is reached, as
        /// specified by the `offset` and `wrap` fields of the
        /// [`PackedEventSuppression`] structure.
        DescriptorIndex = 2,
        /// Reserved value (0x3). Treated as "events enabled" by this
        /// implementation for forward compatibility.
        Reserved = 3,
    }
    impl EventSuppressionFlags {
        const fn into_bits(self) -> u8 {
            self as _
        }
        const fn from_bits(value: u8) -> Self {
            match value {
                0 => Self::Enabled,
                1 => Self::Disabled,
                2 => Self::DescriptorIndex,
                _ => Self::Reserved,
            }
        }
    }

    #[bitfield(u32, repr = u32_le, from = u32_le::new, into = u32_le::get)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PackedEventSuppression {
        #[bits(15)]
        pub offset: u16,
        pub wrap: bool,
        #[bits(2, default = EventSuppressionFlags::Enabled, from = EventSuppressionFlags::from_bits, into = EventSuppressionFlags::into_bits)]
        pub flags: EventSuppressionFlags,
        #[bits(14)]
        _reserved: u16,
    }
}
