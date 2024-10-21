// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

use crate::abi;

/// 64-bit registers
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Register64 {
    InternalActivityState = abi::WHvRegisterInternalActivityState.0,
    DeliverabilityNotifications = abi::WHvRegisterDeliverabilityNotifications.0,

    Pc = abi::WHvArm64RegisterPc.0,

    MpidrEl1 = abi::WHvArm64RegisterMpidrEl1.0,
    Cpsr = abi::WHvArm64RegisterPstate.0,
    Sctlr = abi::WHvArm64RegisterSctlrEl1.0,
    Tcr = abi::WHvArm64RegisterTcrEl1.0,
    Ttbr0 = abi::WHvArm64RegisterTtbr0El1.0,
    Ttbr1 = abi::WHvArm64RegisterTtbr1El1.0,
    Syndrome = abi::WHvArm64RegisterEsrEl1.0,

    GicrBaseGpa = abi::WHvArm64RegisterGicrBaseGpa.0,
}

/// 128-bit registers
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum Register128 {
    PendingEvent = abi::WHvRegisterPendingEvent.0,
    PendingEvent1 = abi::WHvRegisterPendingEvent1.0,
}
