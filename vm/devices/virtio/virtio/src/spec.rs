// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Constants defined by the virtio spec

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

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioDeviceFeaturesBank0 {
    #[bits(24)]
    pub device_specific: u32,
    #[bits(4)]
    _reserved1: u8,
    pub ring_indirect_desc: bool, // VIRTIO_F_INDIRECT_DESC
    pub ring_event_idx: bool,     // VIRTIO_F_EVENT_IDX
    #[bits(2)]
    _reserved2: u8,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioDeviceFeaturesBank1 {
    pub version_1: bool,         // VIRTIO_F_VERSION_1
    pub access_platform: bool,   // VIRTIO_F_ACCESS_PLATFORM
    pub ring_packed: bool,       // VIRTIO_F_RING_PACKED
    pub in_order: bool,          // VIRTIO_F_IN_ORDER
    pub order_platform: bool,    // VIRTIO_F_ORDER_PLATFORM
    pub sriov: bool,             // VIRTIO_F_SR_IOV
    pub notification_data: bool, // VIRTIO_F_NOTIFICATION_DATA
    pub notif_config_data: bool, // VIRTIO_F_NOTIF_CONFIG_DATA
    pub ring_reset: bool,        // VIRTIO_F_RING_RESET
    pub admin_vq: bool,          // VIRTIO_F_ADMIN_VQ
    pub device_specific_bit_42: bool,
    pub suspend: bool, // VIRTIO_F_SUSPEND
    #[bits(7)]
    _reserved: u8,
    #[bits(13)]
    pub device_specific: u16,
}

#[derive(Debug, Clone)]
pub struct VirtioDeviceFeatures(Vec<u32>);
impl VirtioDeviceFeatures {
    pub fn new() -> Self {
        Self(Vec::with_capacity(2))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn set_bank(&mut self, index: usize, val: u32) {
        if self.0.len() <= index {
            self.0.resize(index + 1, 0);
        }
        self.0[index] = val;
    }

    pub fn with_bank(mut self, index: usize, val: u32) -> Self {
        self.set_bank(index, val);
        self
    }

    pub fn with_bank0(self, bank0: VirtioDeviceFeaturesBank0) -> Self {
        self.with_bank(0, bank0.into_bits())
    }

    pub fn with_bank1(self, bank1: VirtioDeviceFeaturesBank1) -> Self {
        self.with_bank(1, bank1.into_bits())
    }

    pub fn bank(&self, index: usize) -> u32 {
        self.0.get(index).map_or(0, |x| *x)
    }

    pub fn bank0(&self) -> VirtioDeviceFeaturesBank0 {
        VirtioDeviceFeaturesBank0::from_bits(self.bank(0))
    }

    pub fn bank1(&self) -> VirtioDeviceFeaturesBank1 {
        VirtioDeviceFeaturesBank1::from_bits(self.bank(1))
    }
}

impl Default for VirtioDeviceFeatures {
    fn default() -> Self {
        Self::new()
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

// ACPI interrupt status flags
pub const VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER: u32 = 1;
pub const VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE: u32 = 2;

/// Virtio over PCI specific constants
pub mod pci {
    pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
    pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
    pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
    pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
    // pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;
    pub const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;

    pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
    pub const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040;
}

/// Virtio queue definitions.
pub mod queue {
    use super::u16_le;
    use super::u32_le;
    use super::u64_le;
    use bitfield_struct::bitfield;

    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct Descriptor {
        pub address: u64_le,
        pub length: u32_le,
        pub flags_raw: u16_le,
        pub next: u16_le,
    }

    impl Descriptor {
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
        #[bits(13)]
        _reserved: u16,
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
}
