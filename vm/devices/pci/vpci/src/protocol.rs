// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::NativeEndian;
use zerocopy::U64;

/// The MMIO page the guest uses to write the target slot number.
pub const MMIO_PAGE_SLOT_NUMBER: u64 = 0;
/// The MMIO page the guest uses to read and write the current slot's config
/// space.
pub const MMIO_PAGE_CONFIG_SPACE: u64 = 0x1000;
/// The mask to apply to an MMIO address to get the page number.
pub const MMIO_PAGE_MASK: u64 = !0xfff;

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageType: u32 {
        BUS_RELATIONS = 0x42490000,
        QUERY_BUS_RELATIONS = 0x42490001,
        INVALIDATE_DEVICE = 0x42490002,
        INVALIDATE_BUS = 0x42490003,
        DEVICE_POWER_STATE_CHANGE = 0x42490004,
        CURRENT_RESOURCE_REQUIREMENTS = 0x42490005,
        GET_RESOURCES = 0x42490006,
        FDO_D0_ENTRY = 0x42490007,
        FDO_D0_EXIT = 0x42490008,
        READ_BLOCK = 0x42490009,
        WRITE_BLOCK = 0x4249000a,
        EJECT = 0x4249000b,
        QUERY_STOP = 0x4249000c,
        RE_ENABLE = 0x4249000d,
        QUERY_STOP_FAILED = 0x4249000e,
        EJECT_COMPLETE = 0x4249000f,
        ASSIGNED_RESOURCES = 0x42490010,
        RELEASE_RESOURCES = 0x42490011,
        INVALIDATE_BLOCK = 0x42490012,
        QUERY_PROTOCOL_VERSION = 0x42490013,
        CREATE_INTERRUPT = 0x42490014,
        DELETE_INTERRUPT = 0x42490015,
        ASSIGNED_RESOURCES2 = 0x42490016,
        CREATE_INTERRUPT2 = 0x42490017,
        DELETE_INTERRUPT2 = 0x42490018,
        BUS_RELATIONS2 = 0x42490019,
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ResourceType: u8 {
        NULL = 0,
        PORT = 1,
        INTERRUPT = 2,
        MEMORY = 3,
        DMA = 4,
        DEVICE_SPECIFIC = 5,
        BUS_NUMBER = 6,
        MEMORY_LARGE = 7,
    }
}

pub const GUID_VPCI_VSP_CHANNEL_TYPE: Guid =
    Guid::from_static_str("44C4F61D-4444-4400-9D52-802E27EDE19F");

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ProtocolVersion: u32 {
        WIN8 = 0x00010000,
        WIN10 = 0x00010001,
        RS1 = 0x00010002,
        VB = 0x00010003,
    }
}

pub const MAXIMUM_PACKET_SIZE: usize = size_of::<DeviceTranslate>()
    + size_of::<MsiResource3>() * MAX_SUPPORTED_INTERRUPT_MESSAGES as usize;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryProtocolVersion {
    pub message_type: MessageType,
    pub protocol_version: ProtocolVersion,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Status: u32 {
        SUCCESS = 0,
        REVISION_MISMATCH = 0xC0000059,
        BAD_DATA = 0xC000090B,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryProtocolVersionReply {
    pub status: Status,
    pub protocol_version: ProtocolVersion,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PnpId {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision_id: u8,
    pub prog_if: u8,
    pub sub_class: u8,
    pub base_class: u8,
    pub sub_vendor_id: u16,
    pub sub_system_id: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceDescription {
    pub pnp_id: PnpId,
    pub slot: SlotNumber,
    pub serial_num: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryBusRelations {
    pub message_type: MessageType,
    pub device_count: u32,
    pub device: DeviceDescription,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceDescription2 {
    pub pnp_id: PnpId,
    pub slot: SlotNumber,
    pub serial_num: u32,
    pub flags: u32,
    pub numa_node: u16,
    pub rsvd: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryBusRelations2 {
    pub message_type: MessageType,
    pub device_count: u32,
    pub device: DeviceDescription2,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SlotNumber {
    #[bits(5)]
    pub device: u8,
    #[bits(3)]
    pub function: u8,
    #[bits(24)]
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryResourceRequirements {
    pub message_type: MessageType,
    pub slot: SlotNumber,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryResourceRequirementsReply {
    pub status: Status,
    pub bars: [u32; 6],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetResources {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub reserved: [u64; 3],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PartialResourceList {
    pub version: u16,
    pub revision: u16,
    pub count: u32,
    pub descriptors: [PartialResourceDescriptor; 6],
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ResourceFlags {
    #[bits(9)]
    pub reserved: u16,
    pub large_40: bool,
    pub large_48: bool,
    pub large_64: bool,
    #[bits(4)]
    pub reserved2: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FdoD0Entry {
    pub message_type: MessageType,
    pub padding: u32,
    pub mmio_start: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PartialResourceDescriptor {
    pub resource_type: ResourceType,
    pub share_disposition: u8,
    pub flags: ResourceFlags,
    pub address: U64<NativeEndian>,
    pub adjusted_len: u32,
    pub padding: u32,
}

// < VPCI_PROTOCOL_VERSION_RS1
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource {
    // union of Descriptor (request) and remap (response)
    pub resource_data: [u64; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceRemapped {
    pub reserved: u16,
    pub message_count: u16,
    pub data_payload: u32,
    pub address: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor {
    pub vector: u8,
    pub delivery_mode: u8,
    pub vector_count: u16,
    pub reserved: [u16; 2],
    pub processor_mask: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor2 {
    pub vector: u8,
    pub delivery_mode: u8,
    pub vector_count: u16,
    pub processor_count: u16,
    pub processor_array: [u16; 32],
    pub reserved: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor3 {
    pub vector: u32,
    pub delivery_mode: u8,
    pub reserved: u8,
    pub vector_count: u16,
    pub processor_count: u16,
    pub processor_array: [u16; 32],
    pub reserved2: u16,
}

impl From<MsiResourceRemapped> for MsiResource {
    fn from(value: MsiResourceRemapped) -> Self {
        let mut this = Self::new_zeroed();
        *this.remapped_mut() = value;
        this
    }
}

impl From<MsiResourceDescriptor> for MsiResource {
    fn from(value: MsiResourceDescriptor) -> Self {
        let mut this = Self::new_zeroed();
        *this.descriptor_mut() = value;
        this
    }
}

impl MsiResource {
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    pub fn descriptor(&self) -> &MsiResourceDescriptor {
        MsiResourceDescriptor::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor {
        MsiResourceDescriptor::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

// >= VPCI_PROTOCOL_VERSION_RS1
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource2 {
    // union of Descriptor (request) and remap (response)
    pub resource_data: [u64; 9],
}

impl From<MsiResourceRemapped> for MsiResource2 {
    fn from(value: MsiResourceRemapped) -> Self {
        let mut this = Self::new_zeroed();
        *this.remapped_mut() = value;
        this
    }
}

impl From<MsiResourceDescriptor2> for MsiResource2 {
    fn from(value: MsiResourceDescriptor2) -> Self {
        let mut this = Self::new_zeroed();
        *this.descriptor_mut() = value;
        this
    }
}

impl MsiResource2 {
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    pub fn descriptor(&self) -> &MsiResourceDescriptor2 {
        MsiResourceDescriptor2::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor2 {
        MsiResourceDescriptor2::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource3 {
    // union of Descriptor (request) and remap (response)
    pub resource_data: [u64; 10],
}

impl From<MsiResourceRemapped> for MsiResource3 {
    fn from(value: MsiResourceRemapped) -> Self {
        let mut this = Self::new_zeroed();
        *this.remapped_mut() = value;
        this
    }
}

impl From<MsiResourceDescriptor3> for MsiResource3 {
    fn from(value: MsiResourceDescriptor3) -> Self {
        let mut this = Self::new_zeroed();
        *this.descriptor_mut() = value;
        this
    }
}

impl MsiResource3 {
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    pub fn descriptor(&self) -> &MsiResourceDescriptor3 {
        MsiResourceDescriptor3::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor3 {
        MsiResourceDescriptor3::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

pub const MAX_SUPPORTED_INTERRUPT_MESSAGES: u32 = 494; // 500 resources minus 6 for the BARs.

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceTranslate {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub mmio_resources: [PartialResourceDescriptor; 6],
    pub msi_resource_count: u32,
    pub reserved: u32,
    // Followed by array of MsiResource/MsiResource2/MsiResource3.
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceTranslateReply {
    pub status: Status,
    pub slot: SlotNumber,
    pub mmio_resources: [PartialResourceDescriptor; 6],
    pub msi_resource_count: u32,
    pub reserved: u32,
    // Followed by array of MsiResource/MsiResource2/MsiResource3.
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterrupt {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub interrupt: MsiResourceDescriptor,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterruptReply {
    pub status: Status,
    pub rsvd: u32,
    pub interrupt: MsiResourceRemapped,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterrupt2 {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub interrupt: MsiResourceDescriptor2,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeleteInterrupt {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub interrupt: MsiResourceRemapped,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DevicePowerChange {
    pub message_type: MessageType,
    pub slot: SlotNumber,
    pub target_state: DevicePowerState,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum DevicePowerState: u32 {
        D0 = 1,
        D3 = 4,
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PdoMessage {
    pub message_type: MessageType,
    pub slot: SlotNumber,
}
