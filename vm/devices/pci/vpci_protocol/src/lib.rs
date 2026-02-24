// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol definitions for Virtual PCI (VPCI) communication.
//!
//! This crate defines the wire protocol used for communication between a virtual PCI
//! bus emulator and its clients. The protocol enables operations such as device enumeration,
//! resource allocation, interrupt management, and power state control.
//!
//! The VPCI protocol is used to create a virtual PCI bus over a vmbus transport, which allows
//! for efficient and safe exposure of PCI devices to guest virtual machines.

#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use std::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::NativeEndian;
use zerocopy::U64;

/// The MMIO page the guest uses to write the target slot number.
///
/// This page is used by the guest to identify which PCI slot it wants to communicate with.
pub const MMIO_PAGE_SLOT_NUMBER: u64 = 0;

/// The MMIO page the guest uses to read and write the current slot's config space.
///
/// After selecting a slot with `MMIO_PAGE_SLOT_NUMBER`, the guest can interact with
/// the selected device's PCI configuration space through this page.
pub const MMIO_PAGE_CONFIG_SPACE: u64 = 0x1000;

/// The mask to apply to an MMIO address to get the page number.
///
/// MMIO operations are aligned to page boundaries, and this mask extracts the page component
/// from an address, zeroing out the offset within the page.
pub const MMIO_PAGE_MASK: u64 = !0xfff;

open_enum! {
    /// Message types used in the VPCI protocol communication.
    ///
    /// These values identify the type of operation being requested or notification being sent.
    /// The message type is included in the header of each protocol message.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageType: u32 {
        /// Bus relations information sent from the VSP to the VSC
        BUS_RELATIONS = 0x42490000,
        /// Request to query bus relations information
        QUERY_BUS_RELATIONS = 0x42490001,
        /// Invalidate a specific device
        INVALIDATE_DEVICE = 0x42490002,
        /// Invalidate the entire bus
        INVALIDATE_BUS = 0x42490003,
        /// Request to change a device's power state
        DEVICE_POWER_STATE_CHANGE = 0x42490004,
        /// Query current resource requirements for a device
        CURRENT_RESOURCE_REQUIREMENTS = 0x42490005,
        /// Get the resources currently assigned to a device
        GET_RESOURCES = 0x42490006,
        /// Notification that a device is entering D0 (powered on) state
        FDO_D0_ENTRY = 0x42490007,
        /// Notification that a device is exiting D0 state
        FDO_D0_EXIT = 0x42490008,
        /// Read a block of data from a device
        READ_BLOCK = 0x42490009,
        /// Write a block of data to a device
        WRITE_BLOCK = 0x4249000a,
        /// Request to eject a device
        EJECT = 0x4249000b,
        /// Query if a device can be stopped
        QUERY_STOP = 0x4249000c,
        /// Re-enable a device that was stopped
        RE_ENABLE = 0x4249000d,
        /// Notification that a query stop operation failed
        QUERY_STOP_FAILED = 0x4249000e,
        /// Notification that a device ejection is complete
        EJECT_COMPLETE = 0x4249000f,
        /// Assigned resources notification for a device
        ASSIGNED_RESOURCES = 0x42490010,
        /// Request to release resources for a device
        RELEASE_RESOURCES = 0x42490011,
        /// Invalidate a block of data
        INVALIDATE_BLOCK = 0x42490012,
        /// Query the protocol version supported
        QUERY_PROTOCOL_VERSION = 0x42490013,
        /// Create an interrupt for a device
        CREATE_INTERRUPT = 0x42490014,
        /// Delete an interrupt for a device
        DELETE_INTERRUPT = 0x42490015,
        /// Assigned resources notification (version 2)
        ASSIGNED_RESOURCES2 = 0x42490016,
        /// Create an interrupt for a device (version 2)
        CREATE_INTERRUPT2 = 0x42490017,
        /// Delete an interrupt for a device (version 2)
        DELETE_INTERRUPT2 = 0x42490018,
        /// Bus relations information (version 2)
        BUS_RELATIONS2 = 0x42490019,
        /// Assigned resources notification (version 3)
        ASSIGNED_RESOURCES3 = 0x4249001a,
        /// Create an interrupt for a device (version 3)
        CREATE_INTERRUPT3 = 0x4249001b,
        /// Reset a device
        RESET_DEVICE = 0x4249001c,
        /// TDISP command from guest to host
        VPCI_TDISP_COMMAND = 0x4249001D,
    }
}

open_enum! {
    /// Types of resources that can be allocated to a PCI device.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ResourceType: u8 {
        /// No resource specified
        NULL = 0,
        /// I/O port resource
        PORT = 1,
        /// Interrupt resource
        INTERRUPT = 2,
        /// Memory resource
        MEMORY = 3,
        /// DMA resource
        DMA = 4,
        /// Device-specific resource
        DEVICE_SPECIFIC = 5,
        /// PCI bus number resource
        BUS_NUMBER = 6,
        /// Large memory resource (for memory regions larger than 4GB)
        MEMORY_LARGE = 7,
    }
}

/// The GUID identifying the VPCI VMBus channel type.
///
/// This GUID is used to identify and establish a VMBus channel for VPCI communication.
pub const GUID_VPCI_VSP_CHANNEL_TYPE: Guid = guid::guid!("44C4F61D-4444-4400-9D52-802E27EDE19F");

open_enum! {
    /// Protocol versions supported by VPCI.
    ///
    /// Each version corresponds to a specific Windows release or feature set.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ProtocolVersion: u32 {
        /// Windows 8 version
        WIN8 = 0x00010000,
        /// Windows 10 version
        WIN10 = 0x00010001,
        /// Windows RS1 (Redstone 1) version
        RS1 = 0x00010002,
        /// Windows VB version
        VB = 0x00010003,
        /// Windows FE version (adds wider vector types for ARM64 interrupts)
        FE = 0x00010004,
        /// Windows GE version (adds device reset)
        GE = 0x00010005,
        /// Windows DT version (allows Windows guests to dynamically map interrupts)
        DT = 0x00010006,
    }
}

/// Maximum size of a packet in the VPCI protocol.
///
/// This constant defines the maximum buffer size needed to hold a complete protocol message,
/// including the largest possible payload (a device translate message with the maximum number
/// of interrupt resources).
pub const MAXIMUM_PACKET_SIZE: usize = size_of::<DeviceTranslate>()
    + size_of::<MsiResource3>() * MAX_SUPPORTED_INTERRUPT_MESSAGES as usize;

/// Message used to query the protocol version.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryProtocolVersion {
    /// Type of message (must be QUERY_PROTOCOL_VERSION)
    pub message_type: MessageType,
    /// The protocol version being queried
    pub protocol_version: ProtocolVersion,
}

open_enum! {
    /// Status codes used in protocol responses.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Status: u32 {
        /// Operation completed successfully
        SUCCESS = 0,
        /// Protocol revision mismatch
        REVISION_MISMATCH = 0xC0000059,
        /// Bad data provided
        BAD_DATA = 0xC000090B,
        /// Operation not supported
        NOT_SUPPORTED = 0xC00000BB,
    }
}

/// Response to a protocol version query.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryProtocolVersionReply {
    /// Status of the version query operation
    pub status: Status,
    /// Protocol version supported by the responder
    pub protocol_version: ProtocolVersion,
}

/// Plug and Play identifier for a PCI device.
///
/// Contains the vendor and device identification information, as well as
/// the device class, subclass, and programming interface information.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PnpId {
    /// PCI vendor ID
    pub vendor_id: u16,
    /// PCI device ID
    pub device_id: u16,
    /// PCI revision ID
    pub revision_id: u8,
    /// PCI programming interface
    pub prog_if: u8,
    /// PCI sub-class code
    pub sub_class: u8,
    /// PCI base class code
    pub base_class: u8,
    /// PCI sub-vendor ID
    pub sub_vendor_id: u16,
    /// PCI subsystem ID
    pub sub_system_id: u16,
}

/// Description of a PCI device in the VPCI bus.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceDescription {
    /// Plug and Play identification information
    pub pnp_id: PnpId,
    /// PCI slot number
    pub slot: SlotNumber,
    /// Device serial number
    pub serial_num: u32,
}

/// Message for querying bus relations.
///
/// This message type is used to report information about devices present on the bus.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryBusRelations {
    /// Type of message (must be BUS_RELATIONS)
    pub message_type: MessageType,
    /// Number of devices reported
    pub device_count: u32,
    /// Alignment for following devices.
    pub device: [DeviceDescription; 0],
}

/// Extended device description (version 2).
///
/// This version adds support for NUMA node information and additional flags.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceDescription2 {
    /// Plug and Play identification information
    pub pnp_id: PnpId,
    /// PCI slot number
    pub slot: SlotNumber,
    /// Device serial number
    pub serial_num: u32,
    /// Device-specific flags
    pub flags: u32,
    /// NUMA node the device is associated with
    pub numa_node: u16,
    /// Reserved field
    pub rsvd: u16,
}

/// Message for querying bus relations (version 2).
///
/// Extended version of the QueryBusRelations message with additional device information.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryBusRelations2 {
    /// Type of message (must be BUS_RELATIONS2)
    pub message_type: MessageType,
    /// Number of devices reported
    pub device_count: u32,
    /// Alignment for following devices.
    pub device: [DeviceDescription2; 0],
}

/// PCI slot number.
///
/// Identifies a specific device and function on the PCI bus.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SlotNumber {
    /// Device number (0-31)
    #[bits(5)]
    pub device: u8,
    /// Function number (0-7)
    #[bits(3)]
    pub function: u8,
    /// Reserved bits
    #[bits(24)]
    pub reserved: u32,
}

/// Message to query resource requirements for a device.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryResourceRequirements {
    /// Type of message (must be CURRENT_RESOURCE_REQUIREMENTS)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
}

/// Response to a resource requirements query.
///
/// Contains information about the BAR (Base Address Register) requirements of a device.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct QueryResourceRequirementsReply {
    /// Status of the query operation
    pub status: Status,
    /// BAR masks for the device's PCI BARs
    pub bars: [u32; 6],
}

/// Message to get currently assigned resources.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetResources {
    /// Type of message (must be GET_RESOURCES)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// Reserved fields
    pub reserved: [u64; 3],
}

/// Resource list for a device.
///
/// Contains descriptors for all resources assigned to a device.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PartialResourceList {
    /// Version of the resource list format
    pub version: u16,
    /// Revision of the resource list format
    pub revision: u16,
    /// Number of descriptors in the list
    pub count: u32,
    /// Resource descriptors for the device's PCI BARs
    pub descriptors: [PartialResourceDescriptor; 6],
}

/// Flags for resource descriptors.
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ResourceFlags {
    /// Reserved bits
    #[bits(9)]
    pub reserved: u16,
    /// Flag for 40-bit large memory
    pub large_40: bool,
    /// Flag for 48-bit large memory
    pub large_48: bool,
    /// Flag for 64-bit large memory
    pub large_64: bool,
    /// Reserved bits
    #[bits(4)]
    pub reserved2: u16,
}

/// Message to notify a device is entering D0 (powered on) state.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FdoD0Entry {
    /// Type of message (must be FDO_D0_ENTRY)
    pub message_type: MessageType,
    /// Padding field
    pub padding: u32,
    /// Base MMIO address for the device
    pub mmio_start: u64,
}

/// Descriptor for a device resource.
///
/// Contains information about a single resource allocated to a device,
/// such as memory, I/O ports, interrupts, etc.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PartialResourceDescriptor {
    /// Type of resource
    pub resource_type: ResourceType,
    /// Sharing disposition
    pub share_disposition: u8,
    /// Resource-specific flags
    pub flags: ResourceFlags,
    /// Base address of the resource
    pub address: U64<NativeEndian>,
    /// Adjusted length of the resource
    pub adjusted_len: u32,
    /// Padding
    pub padding: u32,
}

/// MSI resource descriptor (version 1).
///
/// Used for legacy protocol versions before RS1.
/// Contains a union of either descriptor (request) or remapped (response) data.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource {
    /// Union of descriptor (request) and remap (response) data
    pub resource_data: [u64; 2],
}

/// Remapped MSI resource information.
///
/// Contains the address and data values for a programmed MSI interrupt.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceRemapped {
    /// Reserved field
    pub reserved: u16,
    /// Number of message slots
    pub message_count: u16,
    /// MSI data payload value
    pub data_payload: u32,
    /// MSI address value
    pub address: u64,
}

/// MSI resource descriptor information.
///
/// Contains information about an MSI interrupt request.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor {
    /// Interrupt vector number
    pub vector: u8,
    /// Interrupt delivery mode
    pub delivery_mode: DeliveryMode,
    /// Number of interrupt vectors requested
    pub vector_count: u16,
    /// Reserved fields
    pub reserved: [u16; 2],
    /// Processor mask for interrupt affinity
    pub processor_mask: u64,
}

/// MSI resource descriptor (version 2).
///
/// Enhanced version that supports specifying individual processors
/// rather than using a bit mask.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor2 {
    /// Interrupt vector number
    pub vector: u8,
    /// Interrupt delivery mode
    pub delivery_mode: DeliveryMode,
    /// Number of interrupt vectors requested
    pub vector_count: u16,
    /// Number of processors in the processor_array
    pub processor_count: u16,
    /// Array of processor IDs for interrupt affinity
    pub processor_array: [u16; 32],
    /// Reserved field
    pub reserved: u16,
}

open_enum! {
    /// Interrupt delivery mode
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub enum DeliveryMode: u8 {
        /// Fixed priority delivery mode
        FIXED = 0,
        /// Lowest priority delivery mode (x86 only)
        LOWEST_PRIORITY = 1,
    }
}

/// MSI resource descriptor (version 3).
///
/// Further enhanced version with a full 32-bit vector number.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResourceDescriptor3 {
    /// 32-bit interrupt vector number
    pub vector: u32,
    /// Interrupt delivery mode
    pub delivery_mode: DeliveryMode,
    /// Reserved field
    pub reserved: u8,
    /// Number of interrupt vectors requested
    pub vector_count: u16,
    /// Number of processors in the processor_array
    pub processor_count: u16,
    /// Array of processor IDs for interrupt affinity
    pub processor_array: [u16; 32],
    /// Reserved field
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
    /// Gets a reference to the remapped MSI resource information.
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the remapped MSI resource information.
    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    /// Gets a reference to the MSI descriptor information.
    pub fn descriptor(&self) -> &MsiResourceDescriptor {
        MsiResourceDescriptor::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the MSI descriptor information.
    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor {
        MsiResourceDescriptor::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

/// MSI resource descriptor (version 2).
///
/// Enhanced version used for protocol versions RS1 and later.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource2 {
    /// Union of descriptor (request) and remap (response) data
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
    /// Gets a reference to the remapped MSI resource information.
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the remapped MSI resource information.
    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    /// Gets a reference to the MSI descriptor information.
    pub fn descriptor(&self) -> &MsiResourceDescriptor2 {
        MsiResourceDescriptor2::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the MSI descriptor information.
    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor2 {
        MsiResourceDescriptor2::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

/// MSI resource descriptor (version 3).
///
/// Further enhanced version with extended fields.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MsiResource3 {
    /// Union of descriptor (request) and remap (response) data
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
    /// Gets a reference to the remapped MSI resource information.
    pub fn remapped(&self) -> &MsiResourceRemapped {
        MsiResourceRemapped::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the remapped MSI resource information.
    pub fn remapped_mut(&mut self) -> &mut MsiResourceRemapped {
        MsiResourceRemapped::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    /// Gets a reference to the MSI descriptor information.
    pub fn descriptor(&self) -> &MsiResourceDescriptor3 {
        MsiResourceDescriptor3::ref_from_prefix(self.resource_data.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    /// Gets a mutable reference to the MSI descriptor information.
    pub fn descriptor_mut(&mut self) -> &mut MsiResourceDescriptor3 {
        MsiResourceDescriptor3::mut_from_prefix(self.resource_data.as_mut_bytes())
            .unwrap()
            .0
    } // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

/// Maximum number of interrupt messages supported per device.
///
/// This is calculated as 500 total resources minus 6 for the BARs.
pub const MAX_SUPPORTED_INTERRUPT_MESSAGES: u32 = 494;

/// Message for translating device resources.
///
/// Used to allocate and translate resources for a device.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceTranslate {
    /// Type of message (must be ASSIGNED_RESOURCES or ASSIGNED_RESOURCES2)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// MMIO resource descriptors for the device's PCI BARs
    pub mmio_resources: [PartialResourceDescriptor; 6],
    /// Number of MSI resources to follow
    pub msi_resource_count: u32,
    /// Reserved field
    pub reserved: u32,
    // Followed by array of MsiResource/MsiResource2/MsiResource3.
}

/// Response to a device resource translation request.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeviceTranslateReply {
    /// Status of the translation operation
    pub status: Status,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// Translated MMIO resource descriptors
    pub mmio_resources: [PartialResourceDescriptor; 6],
    /// Number of MSI resources that follow
    pub msi_resource_count: u32,
    /// Reserved field
    pub reserved: u32,
    // Followed by array of MsiResource/MsiResource2/MsiResource3.
}

/// Message to create an interrupt for a device (version 1).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterrupt {
    /// Type of message (must be CREATE_INTERRUPT)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// MSI descriptor for the requested interrupt
    pub interrupt: MsiResourceDescriptor,
}

/// Response to an interrupt creation request.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterruptReply {
    /// Status of the creation operation
    pub status: Status,
    /// Reserved field
    pub rsvd: u32,
    /// Remapped MSI resource for the created interrupt
    pub interrupt: MsiResourceRemapped,
}

/// Message to create an interrupt for a device (version 2).
///
/// Enhanced version that supports specifying individual processors
/// rather than using a bit mask.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterrupt2 {
    /// Type of message (must be CREATE_INTERRUPT2)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// MSI descriptor for the requested interrupt
    pub interrupt: MsiResourceDescriptor2,
}

/// Message to create an interrupt for a device (version 2).
///
/// Enhanced version that supports specifying individual processors
/// rather than using a bit mask.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CreateInterrupt3 {
    /// Type of message (must be CREATE_INTERRUPT3)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// MSI descriptor for the requested interrupt
    pub interrupt: MsiResourceDescriptor3,
}

/// Message to delete an interrupt for a device.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DeleteInterrupt {
    /// Type of message (must be DELETE_INTERRUPT or DELETE_INTERRUPT2)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// Remapped MSI resource for the interrupt to delete
    pub interrupt: MsiResourceRemapped,
}

/// Message to change a device's power state.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DevicePowerChange {
    /// Type of message (must be DEVICE_POWER_STATE_CHANGE)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// Target power state
    pub target_state: DevicePowerState,
}

open_enum! {
    /// Device power states.
    ///
    /// Represents the different power states a device can be in.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum DevicePowerState: u32 {
        /// Device is fully powered and operational
        D0 = 1,
        /// Device is powered off but still enumerated
        D3 = 4,
    }
}

/// Base message format for PDO (Physical Device Object) operations.
///
/// Used for operations like release resources.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PdoMessage {
    /// Type of message
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
}

/// A TDISP packet being sent to the host.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpciTdispCommandHeader {
    /// Type of message (must be VPCI_TDISP_COMMAND)
    pub message_type: MessageType,
    /// PCI slot number of the target device
    pub slot: SlotNumber,
    /// Length of the data payload to follow
    pub data_length: u64,
    // pub data: [u8; data_length...],
}

/// A serialized TDISP VPCI VMBUS command packet.
#[derive(Debug, Clone)]
pub struct VpciTdispCommand {
    /// Header of the VMBUS packet
    pub header: VpciTdispCommandHeader,

    /// The payload of the command (serialized to Vec)
    pub data: Vec<u8>,
}

/// Maximum size of a TDISP command in bytes. Property of the VMBUS implementation on the host.
pub const MAX_VPCI_TDISP_COMMAND_SIZE: usize =
    MAXIMUM_PACKET_SIZE - size_of::<VpciTdispCommandHeader>();
