// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! !! THIS MODULE IS A STUB !!
//!
//! At the moment, we use the PSP emulator running on the host, but construct
//! the corresponding "ASPT" ACPI table within OpenVMM / underhill. Therefore,
//! the only thing you'll find here is just a handful of constants that are
//! required to construct this table.

pub mod reg {
    // ASP Global Registers
    pub const FEATURE: u64 = 0x0000;
    pub const INT_EN: u64 = 0x0004;
    pub const INT_STS: u64 = 0x0008;

    // SEV Mailbox Registers
    pub const CMD_RESP: u64 = 0x0010;
    pub const CMD_BUF_ADDR_LO: u64 = 0x0014;
    pub const CMD_BUF_ADDR_HI: u64 = 0x0018;

    // ACPI Mailbox Registers
    pub const ACPI_CMD_RESP: u64 = 0x0020;
}

pub const PSP_MMIO_ADDRESS: u64 = 0xfeb00000;
