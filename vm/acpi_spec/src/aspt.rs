// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// ACPI definitions for the AMD Secure Processor Table (ASPT).
//
// Used to describe information about the AMD Platform Security Processor (PSP)
// device.

use super::Table;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct Aspt {
    pub num_structs: usize,
    // variable number of trailing structs
}

impl Table for Aspt {
    const SIGNATURE: [u8; 4] = *b"ASPT";
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout)]
pub struct AsptStructHeader {
    pub type_: structs::AsptStructType,
    // length of the struct, including this header
    pub len: u16,
}

impl AsptStructHeader {
    pub fn new<S: structs::AsptStruct>() -> AsptStructHeader {
        AsptStructHeader {
            type_: S::TYPE,
            len: size_of::<S>() as u16,
        }
    }
}

/// Each of these structs is prepended by a `AsptStructHeader`
pub mod structs {
    use open_enum::open_enum;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    open_enum! {
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum AsptStructType: u16 {
            ASP_GLOBAL_REGISTERS = 0,
            SEV_MAILBOX_REGISTERS = 1,
            ACPI_MAILBOX_REGISTERS = 2,
        }
    }

    pub trait AsptStruct: IntoBytes + Immutable + KnownLayout {
        const TYPE: AsptStructType;
    }

    #[repr(C, packed)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct AspGlobalRegisters {
        pub _reserved: u32,
        pub feature_register_address: u64,
        pub interrupt_enable_register_address: u64,
        pub interrupt_status_register_address: u64,
    }

    impl AsptStruct for AspGlobalRegisters {
        const TYPE: AsptStructType = AsptStructType::ASP_GLOBAL_REGISTERS;
    }

    #[repr(C, packed)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct SevMailboxRegisters {
        pub mailbox_interrupt_id: u8,
        pub _reserved: [u8; 3],
        pub cmd_resp_register_address: u64,
        pub cmd_buf_addr_lo_register_address: u64,
        pub cmd_buf_addr_hi_register_address: u64,
    }

    impl AsptStruct for SevMailboxRegisters {
        const TYPE: AsptStructType = AsptStructType::SEV_MAILBOX_REGISTERS;
    }

    #[repr(C, packed)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct AcpiMailboxRegisters {
        pub _reserved1: u32,
        pub cmd_resp_register_address: u64,
        pub _reserved2: [u64; 2],
    }

    impl AsptStruct for AcpiMailboxRegisters {
        const TYPE: AsptStructType = AsptStructType::ACPI_MAILBOX_REGISTERS;
    }
}
