// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the Power State Coordination Interface (PSCI).

use bitfield_struct::bitfield;
use open_enum::open_enum;

pub const PSCI: u8 = 0x4;

#[bitfield(u32)]
#[derive(PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct FastCall {
    pub number: u16,
    pub hint: bool,
    #[bits(7)]
    pub mbz: u8,
    #[bits(6)]
    pub service: u8,
    pub smc64: bool,
    pub fast: bool,
}

open_enum! {
    pub enum PsciCall: FastCall {
        PSCI_VERSION = FastCall(0x8400_0000),
        CPU_SUSPEND = FastCall(0x8400_0001),
        CPU_OFF = FastCall(0x8400_0002),
        CPU_ON = FastCall(0x8400_0003),
        AFFINITY_INFO = FastCall(0x8400_0004),
        MIGRATE = FastCall(0x8400_0005),
        MIGRATE_INFO_TYPE = FastCall(0x8400_0006),
        MIGRATE_INFO_UP_CPU = FastCall(0x8400_0007),
        SYSTEM_OFF = FastCall(0x8400_0008),
        SYSTEM_OFF2 = FastCall(0x8400_0015),
        SYSTEM_RESET = FastCall(0x8400_0009),
        SYSTEM_RESET2 = FastCall(0x8400_0012),
        MEM_PROTECT = FastCall(0x8400_0013),
        MEM_PROTECT_CHECK_RANGE = FastCall(0x8400_0014),
        PSCI_FEATURES = FastCall(0x8400_000a),
        CPU_FREEZE = FastCall(0x8400_000b),
        CPU_DEFAULT_SUSPEND = FastCall(0x8400_000c),
        NODE_HW_STATE = FastCall(0x8400_000d),
        SYSTEM_SUSPEND = FastCall(0x8400_000e),
        PSCI_SET_SUSPEND_MODE = FastCall(0x8400_000f),
        PSCI_STAT_RESIDENCY = FastCall(0x8400_0010),
        PSCI_STAT_COUNT = FastCall(0x8400_0011),
    }
}

open_enum! {
    pub enum PsciError: i32 {
        SUCCESS = 0,
        NOT_SUPPORTED = -1,
        INVALID_PARAMETERS = -2,
        DENIED = -3,
        ALREADY_ON = -4,
        ON_PENDING = -5,
        INTERNAL_FAILURE = -6,
        NOT_PRESENT = -7,
        DISABLED = -8,
        INVALID_ADDRESS = -9,
    }
}
