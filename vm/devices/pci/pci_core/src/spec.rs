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
            NETWORK_CONTROLLER_ETHERNET_GDMA = 0x01,
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
            VENDOR_SPECIFIC = 0x09,
            MSIX            = 0x11,
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
}
