// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Constants defined by the virtio spec

pub use packed_nums::*;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u16_le = zerocopy::U16<zerocopy::LittleEndian>;
    pub type u32_le = zerocopy::U32<zerocopy::LittleEndian>;
    pub type u64_le = zerocopy::U64<zerocopy::LittleEndian>;
}

// Device features - first bank
pub const VIRTIO_F_RING_INDIRECT_DESC: u32 = 0x10000000;
pub const VIRTIO_F_RING_EVENT_IDX: u32 = 0x20000000;
// Device features - second bank
pub const VIRTIO_F_VERSION_1: u32 = 1;

// Device status
pub const VIRTIO_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_DRIVER: u32 = 2;
pub const VIRTIO_DRIVER_OK: u32 = 4;
pub const VIRTIO_FEATURES_OK: u32 = 8;
// const VIRTIO_DEVICE_NEEDS_RESET: u32 = 0x40;
pub const VIRTIO_FAILED: u32 = 0x80;

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
