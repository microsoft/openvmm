// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants specified by the PCI spec.
//!
//! This module MUST NOT contain any vendor-specific constants!

pub mod hwid {
    //! Hardware ID types and constants

    #![expect(missing_docs)] // constants/fields are self-explanatory

    use core::fmt;
    use inspect::Inspect;

    /// A collection of hard-coded hardware IDs specific to a particular PCI
    /// device, as reflected in their corresponding PCI configuration space
    /// registers.
    ///
    /// See PCI 2.3 Spec - 6.2.1 for details on each of these fields.
    #[derive(Debug, Copy, Clone, Inspect)]
    pub struct HardwareIds {
        #[inspect(hex)]
        pub vendor_id: u16,
        #[inspect(hex)]
        pub device_id: u16,
        #[inspect(hex)]
        pub revision_id: u8,
        pub prog_if: ProgrammingInterface,
        pub sub_class: Subclass,
        pub base_class: ClassCode,
        // TODO: this struct should be re-jigged when adding support for other
        // header types (e.g: type 1)
        #[inspect(hex)]
        pub type0_sub_vendor_id: u16,
        #[inspect(hex)]
        pub type0_sub_system_id: u16,
    }

    open_enum::open_enum! {
        /// ClassCode identifies the PCI device's type.
        ///
        /// Values pulled from <https://wiki.osdev.org/PCI#Class_Codes>.
        #[derive(Inspect)]
        #[inspect(display)]
        pub enum ClassCode: u8 {
            UNCLASSIFIED = 0x00,
            MASS_STORAGE_CONTROLLER = 0x01,
            NETWORK_CONTROLLER = 0x02,
            DISPLAY_CONTROLLER = 0x03,
            MULTIMEDIA_CONTROLLER = 0x04,
            MEMORY_CONTROLLER = 0x05,
            BRIDGE = 0x06,
            SIMPLE_COMMUNICATION_CONTROLLER = 0x07,
            BASE_SYSTEM_PERIPHERAL = 0x08,
            INPUT_DEVICE_CONTROLLER = 0x09,
            DOCKING_STATION = 0x0A,
            PROCESSOR = 0x0B,
            SERIAL_BUS_CONTROLLER = 0x0C,
            WIRELESS_CONTROLLER = 0x0D,
            INTELLIGENT_CONTROLLER = 0x0E,
            SATELLITE_COMMUNICATION_CONTROLLER = 0x0F,
            ENCRYPTION_CONTROLLER = 0x10,
            SIGNAL_PROCESSING_CONTROLLER = 0x11,
            PROCESSING_ACCELERATOR = 0x12,
            NONESSENTIAL_INSTRUMENTATION = 0x13,
            // 0x14 - 0x3F: Reserved
            CO_PROCESSOR = 0x40,
            // 0x41 - 0xFE: Reserved
            /// Vendor specific
            UNASSIGNED = 0xFF,
        }
    }

    impl ClassCode {
        pub fn is_reserved(&self) -> bool {
            let c = &self.0;
            (0x14..=0x3f).contains(c) || (0x41..=0xfe).contains(c)
        }
    }

    impl fmt::Display for ClassCode {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.is_reserved() {
                return write!(f, "RESERVED({:#04x})", self.0);
            }
            fmt::Debug::fmt(self, f)
        }
    }

    impl From<u8> for ClassCode {
        fn from(c: u8) -> Self {
            Self(c)
        }
    }

    impl From<ClassCode> for u8 {
        fn from(c: ClassCode) -> Self {
            c.0
        }
    }

    // Most subclass/programming interface values aren't used, and don't have names that can easily be made into variable
    // identifiers (eg, "ISA Compatibility mode controller, supports both channels switched to PCI native mode, supports bus mastering").
    //
    // Therefore, only add values as needed.

    open_enum::open_enum! {
        /// SubclassCode identifies the PCI device's function.
        ///
        /// Values pulled from <https://wiki.osdev.org/PCI#Class_Codes>.
        #[derive(Inspect)]
        #[inspect(transparent(hex))]
        pub enum Subclass: u8 {
            // TODO: As more values are used, add them here.

            NONE = 0x00,

            // Mass Storage Controller (Class code: 0x01)
            MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY = 0x08,

            // Network Controller (Class code: 0x02)
            // Other values: 0x01 - 0x08, 0x80
            NETWORK_CONTROLLER_ETHERNET = 0x00,

            // Bridge (Class code: 0x06)
            // Other values: 0x02 - 0x0A
            BRIDGE_HOST = 0x00,
            BRIDGE_ISA = 0x01,
            BRIDGE_PCI_TO_PCI = 0x04,
            BRIDGE_OTHER = 0x80,

            // Base System Peripheral (Class code: 0x08)
            // Other values: 0x00 - 0x06
            BASE_SYSTEM_PERIPHERAL_OTHER = 0x80,
        }
    }

    impl From<u8> for Subclass {
        fn from(c: u8) -> Self {
            Self(c)
        }
    }

    impl From<Subclass> for u8 {
        fn from(c: Subclass) -> Self {
            c.0
        }
    }

    open_enum::open_enum! {
        /// ProgrammingInterface (aka, program interface byte) identifies the PCI device's
        /// register-level programming interface.
        ///
        /// Values pulled from <https://wiki.osdev.org/PCI#Class_Codes>.
        #[derive(Inspect)]
        #[inspect(transparent(hex))]
        pub enum ProgrammingInterface: u8{
            // TODO: As more values are used, add them here.

            NONE = 0x00,

            // Non-Volatile Memory Controller (Class code:0x01, Subclass: 0x08)
            // Other values: 0x01
            MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY_NVME = 0x02,

            // Ethernet Controller (Class code: 0x02, Subclass: 0x00)
            NETWORK_CONTROLLER_ETHERNET_GDMA = 0x00,
        }
    }

    impl From<u8> for ProgrammingInterface {
        fn from(c: u8) -> Self {
            Self(c)
        }
    }

    impl From<ProgrammingInterface> for u8 {
        fn from(c: ProgrammingInterface) -> Self {
            c.0
        }
    }
}

/// Configuration Space
///
/// Sources: PCI 2.3 Spec - Chapter 6
#[expect(missing_docs)] // primarily enums/structs with self-explanatory variants
pub mod cfg_space {
    use bitfield_struct::bitfield;
    use inspect::Inspect;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    open_enum::open_enum! {
        /// Common configuration space header registers shared between Type 0 and Type 1 headers.
        ///
        /// These registers appear at the same offsets in both header types and have the same
        /// meaning and format.
        ///
        /// | Offset | Bits 31-24     | Bits 23-16  | Bits 15-8   | Bits 7-0             |
        /// |--------|----------------|-------------|-------------|----------------------|
        /// | 0x0    | Device ID      |             | Vendor ID   |                      |
        /// | 0x4    | Status         |             | Command     |                      |
        /// | 0x8    | Class code     |             |             | Revision ID          |
        /// | 0x34   | Reserved       |             |             | Capabilities Pointer |
        pub enum CommonHeader: u16 {
            DEVICE_VENDOR       = 0x00,
            STATUS_COMMAND      = 0x04,
            CLASS_REVISION      = 0x08,
            RESERVED_CAP_PTR    = 0x34,
        }
    }

    /// Size of the common header portion shared by all PCI header types.
    pub const COMMON_HEADER_SIZE: u16 = 0x10;

    open_enum::open_enum! {
        /// Offsets into the type 00h configuration space header.
        ///
        /// Table pulled from <https://wiki.osdev.org/PCI>
        ///
        /// | Offset | Bits 31-24                 | Bits 23-16  | Bits 15-8           | Bits 7-0             |
        /// |--------|----------------------------|-------------|---------------------|--------------------- |
        /// | 0x0    | Device ID                  |             | Vendor ID           |                      |
        /// | 0x4    | Status                     |             | Command             |                      |
        /// | 0x8    | Class code                 |             |                     | Revision ID          |
        /// | 0xC    | BIST                       | Header type | Latency Timer       | Cache Line Size      |
        /// | 0x10   | Base address #0 (BAR0)     |             |                     |                      |
        /// | 0x14   | Base address #1 (BAR1)     |             |                     |                      |
        /// | 0x18   | Base address #2 (BAR2)     |             |                     |                      |
        /// | 0x1C   | Base address #3 (BAR3)     |             |                     |                      |
        /// | 0x20   | Base address #4 (BAR4)     |             |                     |                      |
        /// | 0x24   | Base address #5 (BAR5)     |             |                     |                      |
        /// | 0x28   | Cardbus CIS Pointer        |             |                     |                      |
        /// | 0x2C   | Subsystem ID               |             | Subsystem Vendor ID |                      |
        /// | 0x30   | Expansion ROM base address |             |                     |                      |
        /// | 0x34   | Reserved                   |             |                     | Capabilities Pointer |
        /// | 0x38   | Reserved                   |             |                     |                      |
        /// | 0x3C   | Max latency                | Min Grant   | Interrupt PIN       | Interrupt Line       |
        pub enum HeaderType00: u16 {
            DEVICE_VENDOR      = 0x00,
            STATUS_COMMAND     = 0x04,
            CLASS_REVISION     = 0x08,
            BIST_HEADER        = 0x0C,
            BAR0               = 0x10,
            BAR1               = 0x14,
            BAR2               = 0x18,
            BAR3               = 0x1C,
            BAR4               = 0x20,
            BAR5               = 0x24,
            CARDBUS_CIS_PTR    = 0x28,
            SUBSYSTEM_ID       = 0x2C,
            EXPANSION_ROM_BASE = 0x30,
            RESERVED_CAP_PTR   = 0x34,
            RESERVED           = 0x38,
            LATENCY_INTERRUPT  = 0x3C,
        }
    }

    pub const HEADER_TYPE_00_SIZE: u16 = 0x40;

    open_enum::open_enum! {
        /// Offsets into the type 01h configuration space header.
        ///
        /// Table pulled from <https://wiki.osdev.org/PCI>
        ///
        /// | Offset | Bits 31-24                       | Bits 23-16             | Bits 15-8                | Bits 7-0             |
        /// |--------|----------------------------------|------------------------|--------------------------|--------------------- |
        /// | 0x0    | Device ID                        |                        | Vendor ID                |                      |
        /// | 0x4    | Status                           |                        | Command                  |                      |
        /// | 0x8    | Class code                       |                        |                          | Revision ID          |
        /// | 0xC    | BIST                             | Header Type            | Latency Timer            | Cache Line Size      |
        /// | 0x10   | Base address #0 (BAR0)           |                        |                          |                      |
        /// | 0x14   | Base address #1 (BAR1)           |                        |                          |                      |
        /// | 0x18   | Secondary Latency Timer          | Subordinate Bus Number | Secondary Bus Number     | Primary Bus Number   |
        /// | 0x1C   | Secondary Status                 |                        | I/O Limit                | I/O Base             |
        /// | 0x20   | Memory Limit                     |                        | Memory Base              |                      |
        /// | 0x24   | Prefetchable Memory Limit        |                        | Prefetchable Memory Base |                      |
        /// | 0x28   | Prefetchable Base Upper 32 Bits  |                        |                          |                      |
        /// | 0x2C   | Prefetchable Limit Upper 32 Bits |                        |                          |                      |
        /// | 0x30   | I/O Limit Upper 16 Bits          |                        | I/O Base Upper 16 Bits   |                      |
        /// | 0x34   | Reserved                         |                        |                          | Capabilities Pointer |
        /// | 0x38   | Expansion ROM Base Address       |                        |                          |                      |
        /// | 0x3C   | Bridge Control                   |                        | Interrupt PIN            | Interrupt Line       |
        pub enum HeaderType01: u16 {
            DEVICE_VENDOR         = 0x00,
            STATUS_COMMAND        = 0x04,
            CLASS_REVISION        = 0x08,
            BIST_HEADER           = 0x0C,
            BAR0                  = 0x10,
            BAR1                  = 0x14,
            LATENCY_BUS_NUMBERS   = 0x18,
            SEC_STATUS_IO_RANGE   = 0x1C,
            MEMORY_RANGE          = 0x20,
            PREFETCH_RANGE        = 0x24,
            PREFETCH_BASE_UPPER   = 0x28,
            PREFETCH_LIMIT_UPPER  = 0x2C,
            IO_RANGE_UPPER        = 0x30,
            RESERVED_CAP_PTR      = 0x34,
            EXPANSION_ROM_BASE    = 0x38,
            BRDIGE_CTRL_INTERRUPT = 0x3C,
        }
    }

    pub const HEADER_TYPE_01_SIZE: u16 = 0x40;

    /// BAR in-band encoding bits.
    ///
    /// The low bits of the BAR are not actually part of the address.
    /// Instead, they are used to in-band encode various bits of
    /// metadata about the BAR, and are masked off when determining the
    /// actual address.
    #[bitfield(u32)]
    pub struct BarEncodingBits {
        pub use_pio: bool,

        _reserved: bool,

        /// False indicates 32 bit.
        /// Only used in MMIO
        pub type_64_bit: bool,
        pub prefetchable: bool,

        #[bits(28)]
        _reserved2: u32,
    }

    /// Command Register
    #[derive(Inspect)]
    #[bitfield(u16)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct Command {
        pub pio_enabled: bool,
        pub mmio_enabled: bool,
        pub bus_master: bool,
        pub special_cycles: bool,
        pub enable_memory_write_invalidate: bool,
        pub vga_palette_snoop: bool,
        pub parity_error_response: bool,
        /// must be 0
        #[bits(1)]
        _reserved: u16,
        pub enable_serr: bool,
        pub enable_fast_b2b: bool,
        pub intx_disable: bool,
        #[bits(5)]
        _reserved2: u16,
    }

    /// Status Register
    #[bitfield(u16)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct Status {
        #[bits(3)]
        _reserved: u16,
        pub interrupt_status: bool,
        pub capabilities_list: bool,
        pub capable_mhz_66: bool,
        _reserved2: bool,
        pub capable_fast_b2b: bool,
        pub err_master_parity: bool,

        #[bits(2)]
        pub devsel: DevSel,

        pub abort_target_signaled: bool,
        pub abort_target_received: bool,
        pub abort_master_received: bool,
        pub err_signaled: bool,
        pub err_detected_parity: bool,
    }

    #[derive(Debug)]
    #[repr(u16)]
    pub enum DevSel {
        Fast = 0b00,
        Medium = 0b01,
        Slow = 0b10,
    }

    impl DevSel {
        const fn from_bits(bits: u16) -> Self {
            match bits {
                0b00 => DevSel::Fast,
                0b01 => DevSel::Medium,
                0b10 => DevSel::Slow,
                _ => unreachable!(),
            }
        }

        const fn into_bits(self) -> u16 {
            self as u16
        }
    }
}

/// Capabilities
pub mod caps {
    open_enum::open_enum! {
        /// Capability IDs
        ///
        /// Sources: PCI 2.3 Spec - Appendix H
        ///
        /// NOTE: this is a non-exhaustive list, so don't be afraid to add new
        /// variants on an as-needed basis!
        pub enum CapabilityId: u8 {
            #![expect(missing_docs)] // self explanatory variants
            MSI             = 0x05,
            VENDOR_SPECIFIC = 0x09,
            PCI_EXPRESS     = 0x10,
            MSIX            = 0x11,
        }
    }

    /// MSI
    #[expect(missing_docs)] // primarily enums/structs with self-explanatory variants
    pub mod msi {
        open_enum::open_enum! {
            /// Offsets into the MSI Capability Header
            ///
            /// Based on PCI Local Bus Specification Rev 3.0, Section 6.8.1
            ///
            /// | Offset    | Bits 31-24    | Bits 23-16    | Bits 15-8     | Bits 7-0              |
            /// |-----------|---------------|---------------|---------------|-----------------------|
            /// | Cap + 0x0 | Message Control               | Next Pointer  | Capability ID (0x05)  |
            /// | Cap + 0x4 | Message Address (32-bit or lower 32-bit of 64-bit)                    |
            /// | Cap + 0x8 | Message Address Upper 32-bit (64-bit capable only)                    |
            /// | Cap + 0xC | Message Data  |               |               |                       |
            /// | Cap + 0x10| Mask Bits (Per-vector masking capable only)                           |
            /// | Cap + 0x14| Pending Bits (Per-vector masking capable only)                        |
            pub enum MsiCapabilityHeader: u16 {
                CONTROL_CAPS = 0x00,
                MSG_ADDR_LO  = 0x04,
                MSG_ADDR_HI  = 0x08,
                MSG_DATA_32  = 0x08,  // For 32-bit address capable
                MSG_DATA_64  = 0x0C,  // For 64-bit address capable
                MASK_BITS    = 0x10,  // 64-bit + per-vector masking
                PENDING_BITS = 0x14,  // 64-bit + per-vector masking
            }
        }
    }

    /// MSI-X
    #[expect(missing_docs)] // primarily enums/structs with self-explanatory variants
    pub mod msix {
        open_enum::open_enum! {
            /// Offsets into the MSI-X Capability Header
            ///
            /// Table pulled from <https://wiki.osdev.org/PCI>
            ///
            /// | Offset    | Bits 31-24         | Bits 23-16 | Bits 15-8    | Bits 7-3             | Bits 2-0 |
            /// |-----------|--------------------|------------|--------------|----------------------|----------|
            /// | Cap + 0x0 | Message Control    |            | Next Pointer | Capability ID (0x11) |          |
            /// | Cap + 0x4 | Table Offset       |            |              |                      | BIR      |
            /// | Cap + 0x8 | Pending Bit Offset |            |              |                      | BIR      |
            pub enum MsixCapabilityHeader: u16 {
                CONTROL_CAPS = 0x00,
                OFFSET_TABLE = 0x04,
                OFFSET_PBA   = 0x08,
            }
        }

        open_enum::open_enum! {
            /// Offsets into a single MSI-X Table Entry
            pub enum MsixTableEntryIdx: u16 {
                MSG_ADDR_LO = 0x00,
                MSG_ADDR_HI = 0x04,
                MSG_DATA    = 0x08,
                VECTOR_CTL  = 0x0C,
            }
        }
    }

    /// PCI Express
    #[expect(missing_docs)] // primarily enums/structs with self-explanatory variants
    pub mod pci_express {
        use bitfield_struct::bitfield;
        use inspect::Inspect;
        use zerocopy::FromBytes;
        use zerocopy::Immutable;
        use zerocopy::IntoBytes;
        use zerocopy::KnownLayout;

        /// PCIe Link Speed encoding values for use in Link Capabilities and other registers.
        ///
        /// Values are defined in PCIe Base Specification for the Max Link Speed field
        /// in Link Capabilities Register and similar fields.
        #[derive(Debug)]
        #[repr(u32)]
        pub enum LinkSpeed {
            /// 2.5 GT/s link speed
            Speed2_5GtS = 0b0001,
            /// 5.0 GT/s link speed
            Speed5_0GtS = 0b0010,
            /// 8.0 GT/s link speed
            Speed8_0GtS = 0b0011,
            /// 16.0 GT/s link speed
            Speed16_0GtS = 0b0100,
            /// 32.0 GT/s link speed
            Speed32_0GtS = 0b0101,
            /// 64.0 GT/s link speed
            Speed64_0GtS = 0b0110,
            // All other encodings are reserved
        }

        impl LinkSpeed {
            pub const fn from_bits(bits: u32) -> Self {
                match bits {
                    0b0001 => LinkSpeed::Speed2_5GtS,
                    0b0010 => LinkSpeed::Speed5_0GtS,
                    0b0011 => LinkSpeed::Speed8_0GtS,
                    0b0100 => LinkSpeed::Speed16_0GtS,
                    0b0101 => LinkSpeed::Speed32_0GtS,
                    0b0110 => LinkSpeed::Speed64_0GtS,
                    _ => unreachable!(),
                }
            }

            pub const fn into_bits(self) -> u32 {
                self as u32
            }
        }

        /// PCIe Supported Link Speeds Vector encoding values for use in Link Capabilities 2 register.
        ///
        /// Values are defined in PCIe Base Specification for the Supported Link Speeds Vector field
        /// in Link Capabilities 2 Register. Each bit represents support for a specific generation.
        #[derive(Debug)]
        #[repr(u32)]
        pub enum SupportedLinkSpeedsVector {
            /// Support up to Gen 1 (2.5 GT/s)
            UpToGen1 = 0b0000001,
            /// Support up to Gen 2 (5.0 GT/s)
            UpToGen2 = 0b0000011,
            /// Support up to Gen 3 (8.0 GT/s)
            UpToGen3 = 0b0000111,
            /// Support up to Gen 4 (16.0 GT/s)
            UpToGen4 = 0b0001111,
            /// Support up to Gen 5 (32.0 GT/s)
            UpToGen5 = 0b0011111,
            /// Support up to Gen 6 (64.0 GT/s)
            UpToGen6 = 0b0111111,
            // All other encodings are reserved
        }

        impl SupportedLinkSpeedsVector {
            pub const fn from_bits(bits: u32) -> Self {
                match bits {
                    0b0000001 => SupportedLinkSpeedsVector::UpToGen1,
                    0b0000011 => SupportedLinkSpeedsVector::UpToGen2,
                    0b0000111 => SupportedLinkSpeedsVector::UpToGen3,
                    0b0001111 => SupportedLinkSpeedsVector::UpToGen4,
                    0b0011111 => SupportedLinkSpeedsVector::UpToGen5,
                    0b0111111 => SupportedLinkSpeedsVector::UpToGen6,
                    _ => unreachable!(),
                }
            }

            pub const fn into_bits(self) -> u32 {
                self as u32
            }
        }

        /// PCIe Link Width encoding values for use in Link Capabilities and other registers.
        ///
        /// Values are defined in PCIe Base Specification for the Max Link Width field
        /// in Link Capabilities Register and similar fields.
        #[derive(Debug)]
        #[repr(u32)]
        pub enum LinkWidth {
            /// x1 link width
            X1 = 0b000001,
            /// x2 link width
            X2 = 0b000010,
            /// x4 link width
            X4 = 0b000100,
            /// x8 link width
            X8 = 0b001000,
            /// x16 link width
            X16 = 0b010000,
            // All other encodings are reserved
        }

        impl LinkWidth {
            pub const fn from_bits(bits: u32) -> Self {
                match bits {
                    0b000001 => LinkWidth::X1,
                    0b000010 => LinkWidth::X2,
                    0b000100 => LinkWidth::X4,
                    0b001000 => LinkWidth::X8,
                    0b010000 => LinkWidth::X16,
                    _ => unreachable!(),
                }
            }

            pub const fn into_bits(self) -> u32 {
                self as u32
            }
        }

        open_enum::open_enum! {
            /// Offsets into the PCI Express Capability Header
            ///
            /// Table pulled from PCI Express Base Specification Rev. 3.0
            ///
            /// | Offset    | Bits 31-24       | Bits 23-16       | Bits 15-8        | Bits 7-0             |
            /// |-----------|------------------|----------------- |------------------|----------------------|
            /// | Cap + 0x0 | PCI Express Capabilities Register   | Next Pointer     | Capability ID (0x10) |
            /// | Cap + 0x4 | Device Capabilities Register                                                  |
            /// | Cap + 0x8 | Device Status    | Device Control                                             |
            /// | Cap + 0xC | Link Capabilities Register                                                    |
            /// | Cap + 0x10| Link Status      | Link Control                                               |
            /// | Cap + 0x14| Slot Capabilities Register                                                    |
            /// | Cap + 0x18| Slot Status      | Slot Control                                               |
            /// | Cap + 0x1C| Root Capabilities| Root Control                                               |
            /// | Cap + 0x20| Root Status Register                                                          |
            /// | Cap + 0x24| Device Capabilities 2 Register                                                |
            /// | Cap + 0x28| Device Status 2  | Device Control 2                                           |
            /// | Cap + 0x2C| Link Capabilities 2 Register                                                  |
            /// | Cap + 0x30| Link Status 2    | Link Control 2                                             |
            /// | Cap + 0x34| Slot Capabilities 2 Register                                                  |
            /// | Cap + 0x38| Slot Status 2    | Slot Control 2                                             |
            pub enum PciExpressCapabilityHeader: u16 {
                PCIE_CAPS           = 0x00,
                DEVICE_CAPS         = 0x04,
                DEVICE_CTL_STS      = 0x08,
                LINK_CAPS           = 0x0C,
                LINK_CTL_STS        = 0x10,
                SLOT_CAPS           = 0x14,
                SLOT_CTL_STS        = 0x18,
                ROOT_CTL_CAPS       = 0x1C,
                ROOT_STS            = 0x20,
                DEVICE_CAPS_2       = 0x24,
                DEVICE_CTL_STS_2    = 0x28,
                LINK_CAPS_2         = 0x2C,
                LINK_CTL_STS_2      = 0x30,
                SLOT_CAPS_2         = 0x34,
                SLOT_CTL_STS_2      = 0x38,
            }
        }

        /// PCI Express Capabilities Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct PciExpressCapabilities {
            #[bits(4)]
            pub capability_version: u16,
            #[bits(4)]
            pub device_port_type: DevicePortType,
            pub slot_implemented: bool,
            #[bits(5)]
            pub interrupt_message_number: u16,
            pub _undefined: bool,
            pub flit_mode_supported: bool,
        }

        #[derive(Debug)]
        #[repr(u16)]
        pub enum DevicePortType {
            Endpoint = 0b0000,
            RootPort = 0b0100,
            UpstreamSwitchPort = 0b0101,
            DownstreamSwitchPort = 0b0110,
        }

        impl DevicePortType {
            const fn from_bits(bits: u16) -> Self {
                match bits {
                    0b0000 => DevicePortType::Endpoint,
                    0b0100 => DevicePortType::RootPort,
                    0b0101 => DevicePortType::UpstreamSwitchPort,
                    0b0110 => DevicePortType::DownstreamSwitchPort,
                    _ => unreachable!(),
                }
            }

            const fn into_bits(self) -> u16 {
                self as u16
            }
        }

        /// Device Capabilities Register (From the 6.4 spec)
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceCapabilities {
            #[bits(3)]
            pub max_payload_size: u32,
            #[bits(2)]
            pub phantom_functions: u32,
            pub ext_tag_field: bool,
            #[bits(3)]
            pub endpoint_l0s_latency: u32,
            #[bits(3)]
            pub endpoint_l1_latency: u32,
            #[bits(3)]
            _reserved1: u32,
            pub role_based_error: bool,
            pub err_cor_subclass_capable: bool,
            pub rx_mps_fixed: bool,
            #[bits(8)]
            pub captured_slot_power_limit: u32,
            #[bits(2)]
            pub captured_slot_power_scale: u32,
            pub function_level_reset: bool,
            pub mixed_mps_supported: bool,
            pub tee_io_supported: bool,
            _reserved3: bool,
        }

        /// Device Control Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceControl {
            pub correctable_error_reporting_enable: bool,
            pub non_fatal_error_reporting_enable: bool,
            pub fatal_error_reporting_enable: bool,
            pub unsupported_request_reporting_enable: bool,
            pub enable_relaxed_ordering: bool,
            #[bits(3)]
            pub max_payload_size: u16,
            pub extended_tag_enable: bool,
            pub phantom_functions_enable: bool,
            pub aux_power_pm_enable: bool,
            pub enable_no_snoop: bool,
            #[bits(3)]
            pub max_read_request_size: u16,
            pub initiate_function_level_reset: bool,
        }

        /// Device Status Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceStatus {
            pub correctable_error_detected: bool,
            pub non_fatal_error_detected: bool,
            pub fatal_error_detected: bool,
            pub unsupported_request_detected: bool,
            pub aux_power_detected: bool,
            pub transactions_pending: bool,
            #[bits(10)]
            _reserved: u16,
        }

        /// Link Capabilities Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkCapabilities {
            #[bits(4)]
            pub max_link_speed: u32,
            #[bits(6)]
            pub max_link_width: u32,
            #[bits(2)]
            pub aspm_support: u32,
            #[bits(3)]
            pub l0s_exit_latency: u32,
            #[bits(3)]
            pub l1_exit_latency: u32,
            pub clock_power_management: bool,
            pub surprise_down_error_reporting: bool,
            pub data_link_layer_link_active_reporting: bool,
            pub link_bandwidth_notification_capability: bool,
            pub aspm_optionality_compliance: bool,
            #[bits(1)]
            _reserved: u32,
            #[bits(8)]
            pub port_number: u32,
        }

        /// Link Control Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkControl {
            #[bits(2)]
            pub aspm_control: u16,
            pub ptm_propagation_delay_adaptation_interpretation_b: bool,
            #[bits(1)]
            pub read_completion_boundary: u16,
            pub link_disable: bool,
            pub retrain_link: bool,
            pub common_clock_configuration: bool,
            pub extended_synch: bool,
            pub enable_clock_power_management: bool,
            pub hardware_autonomous_width_disable: bool,
            pub link_bandwidth_management_interrupt_enable: bool,
            pub link_autonomous_bandwidth_interrupt_enable: bool,
            #[bits(1)]
            pub sris_clocking: u16,
            pub flit_mode_disable: bool,
            #[bits(2)]
            pub drs_signaling_control: u16,
        }

        /// Link Status Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkStatus {
            #[bits(4)]
            pub current_link_speed: u16,
            #[bits(6)]
            pub negotiated_link_width: u16,
            #[bits(1)]
            _reserved: u16,
            pub link_training: bool,
            pub slot_clock_configuration: bool,
            pub data_link_layer_link_active: bool,
            pub link_bandwidth_management_status: bool,
            pub link_autonomous_bandwidth_status: bool,
        }

        /// Slot Capabilities Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotCapabilities {
            pub attention_button_present: bool,
            pub power_controller_present: bool,
            pub mrl_sensor_present: bool,
            pub attention_indicator_present: bool,
            pub power_indicator_present: bool,
            pub hot_plug_surprise: bool,
            pub hot_plug_capable: bool,
            #[bits(8)]
            pub slot_power_limit_value: u32,
            #[bits(2)]
            pub slot_power_limit_scale: u32,
            pub electromechanical_interlock_present: bool,
            pub no_command_completed_support: bool,
            #[bits(13)]
            pub physical_slot_number: u32,
        }

        /// Slot Control Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotControl {
            pub attention_button_pressed_enable: bool,
            pub power_fault_detected_enable: bool,
            pub mrl_sensor_changed_enable: bool,
            pub presence_detect_changed_enable: bool,
            pub command_completed_interrupt_enable: bool,
            pub hot_plug_interrupt_enable: bool,
            #[bits(2)]
            pub attention_indicator_control: u16,
            #[bits(2)]
            pub power_indicator_control: u16,
            pub power_controller_control: bool,
            pub electromechanical_interlock_control: bool,
            pub data_link_layer_state_changed_enable: bool,
            pub auto_slot_power_limit_enable: bool,
            pub in_band_pd_disable: bool,
            #[bits(1)]
            _reserved: u16,
        }

        /// Slot Status Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotStatus {
            pub attention_button_pressed: bool,
            pub power_fault_detected: bool,
            pub mrl_sensor_changed: bool,
            pub presence_detect_changed: bool,
            pub command_completed: bool,
            #[bits(1)]
            pub mrl_sensor_state: u16,
            #[bits(1)]
            pub presence_detect_state: u16,
            #[bits(1)]
            pub electromechanical_interlock_status: u16,
            pub data_link_layer_state_changed: bool,
            #[bits(7)]
            _reserved: u16,
        }

        /// Root Control Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct RootControl {
            pub system_error_on_correctable_error_enable: bool,
            pub system_error_on_non_fatal_error_enable: bool,
            pub system_error_on_fatal_error_enable: bool,
            pub pme_interrupt_enable: bool,
            pub crs_software_visibility_enable: bool,
            pub no_nfm_subtree_below_this_root_port: bool,
            #[bits(10)]
            _reserved: u16,
        }

        /// Root Capabilities Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct RootCapabilities {
            pub crs_software_visibility: bool,
            #[bits(15)]
            _reserved: u16,
        }

        /// Root Status Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct RootStatus {
            #[bits(16)]
            pub pme_requester_id: u32,
            pub pme_status: bool,
            pub pme_pending: bool,
            #[bits(14)]
            _reserved: u32,
        }

        /// Device Capabilities 2 Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceCapabilities2 {
            #[bits(4)]
            pub completion_timeout_ranges_supported: u32,
            pub completion_timeout_disable_supported: bool,
            pub ari_forwarding_supported: bool,
            pub atomic_op_routing_supported: bool,
            pub atomic_op_32_bit_completer_supported: bool,
            pub atomic_op_64_bit_completer_supported: bool,
            pub cas_128_bit_completer_supported: bool,
            pub no_ro_enabled_pr_pr_passing: bool,
            pub ltr_mechanism_supported: bool,
            #[bits(2)]
            pub tph_completer_supported: u32,
            #[bits(2)]
            _reserved: u32,
            pub ten_bit_tag_completer_supported: bool,
            pub ten_bit_tag_requester_supported: bool,
            #[bits(2)]
            pub obff_supported: u32,
            pub extended_fmt_field_supported: bool,
            pub end_end_tlp_prefix_supported: bool,
            #[bits(2)]
            pub max_end_end_tlp_prefixes: u32,
            #[bits(2)]
            pub emergency_power_reduction_supported: u32,
            pub emergency_power_reduction_init_required: bool,
            #[bits(1)]
            _reserved: u32,
            pub dmwr_completer_supported: bool,
            #[bits(2)]
            pub dmwr_lengths_supported: u32,
            pub frs_supported: bool,
        }

        /// Device Control 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceControl2 {
            #[bits(4)]
            pub completion_timeout_value: u16,
            pub completion_timeout_disable: bool,
            pub ari_forwarding_enable: bool,
            pub atomic_op_requester_enable: bool,
            pub atomic_op_egress_blocking: bool,
            pub ido_request_enable: bool,
            pub ido_completion_enable: bool,
            pub ltr_mechanism_enable: bool,
            pub emergency_power_reduction_request: bool,
            pub ten_bit_tag_requester_enable: bool,
            #[bits(2)]
            pub obff_enable: u16,
            pub end_end_tlp_prefix_blocking: bool,
        }

        /// Device Status 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct DeviceStatus2 {
            #[bits(16)]
            _reserved: u16,
        }

        /// Link Capabilities 2 Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkCapabilities2 {
            #[bits(1)]
            _reserved: u32,
            #[bits(7)]
            pub supported_link_speeds_vector: u32,
            pub crosslink_supported: bool,
            #[bits(7)]
            pub lower_skp_os_generation_supported_speeds_vector: u32,
            #[bits(7)]
            pub lower_skp_os_reception_supported_speeds_vector: u32,
            pub retimer_presence_detect_supported: bool,
            pub two_retimers_presence_detect_supported: bool,
            #[bits(6)]
            _reserved: u32,
            pub drs_supported: bool,
        }

        /// Link Control 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkControl2 {
            #[bits(4)]
            pub target_link_speed: u16,
            pub enter_compliance: bool,
            pub hardware_autonomous_speed_disable: bool,
            #[bits(1)]
            pub selectable_de_emphasis: u16,
            #[bits(3)]
            pub transmit_margin: u16,
            pub enter_modified_compliance: bool,
            pub compliance_sos: bool,
            #[bits(4)]
            pub compliance_preset_de_emphasis: u16,
        }

        /// Link Status 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct LinkStatus2 {
            #[bits(1)]
            pub current_de_emphasis_level: u16,
            pub equalization_8gts_complete: bool,
            pub equalization_8gts_phase_1_successful: bool,
            pub equalization_8gts_phase_2_successful: bool,
            pub equalization_8gts_phase_3_successful: bool,
            pub link_equalization_request_8gts: bool,
            pub retimer_presence_detected: bool,
            pub two_retimers_presence_detected: bool,
            #[bits(2)]
            pub crosslink_resolution: u16,
            pub flit_mode_status: bool,
            #[bits(1)]
            _reserved: u16,
            #[bits(3)]
            pub downstream_component_presence: u16,
            pub drs_message_received: bool,
        }

        /// Slot Capabilities 2 Register
        #[bitfield(u32)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotCapabilities2 {
            pub in_band_pd_disable_supported: bool,
            #[bits(31)]
            _reserved: u32,
        }

        /// Slot Control 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotControl2 {
            #[bits(16)]
            _reserved: u16,
        }

        /// Slot Status 2 Register
        #[bitfield(u16)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
        pub struct SlotStatus2 {
            #[bits(16)]
            _reserved: u16,
        }
    }
}
