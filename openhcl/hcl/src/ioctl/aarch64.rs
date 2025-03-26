// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for non-hardware-isolated ARM64 partitions.

use super::HclVp;
use super::NoRunner;
use super::ProcessorRunner;
use crate::protocol::hcl_cpu_context_aarch64;
use crate::GuestVtl;
use hvdef::HvArm64RegisterName;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use sidecar_client::SidecarVp;
use std::ptr::addr_of;
use std::ptr::addr_of_mut;
use thiserror::Error;

/// Result when the translate gva hypercall returns a code indicating
/// the translation was unsuccessful.
#[derive(Error, Debug)]
#[error("translate gva to gpa returned non-successful code {code:?}")]
pub struct TranslateErrorAarch64 {
    /// The code returned by the translate gva hypercall.
    pub code: u32,
}

/// Runner backing for non-hardware-isolated ARM64 partitions.
#[non_exhaustive]
pub struct MshvArm64 {}

impl ProcessorRunner<'_, MshvArm64> {
    /// Returns a reference to the current VTL's CPU context.
    pub fn cpu_context(&self) -> &hcl_cpu_context_aarch64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &*addr_of!((*self.run.as_ptr()).context).cast() }
    }

    /// Returns a mutable reference to the current VTL's CPU context.
    pub fn cpu_context_mut(&mut self) -> &mut hcl_cpu_context_aarch64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &mut *addr_of_mut!((*self.run.as_ptr()).context).cast() }
    }
}

impl super::BackingPrivate for MshvArm64 {
    fn new(vp: &HclVp, sidecar: Option<&SidecarVp<'_>>) -> Result<Self, NoRunner> {
        assert!(sidecar.is_none());
        let super::BackingState::Mshv { reg_page: _ } = &vp.backing else {
            unreachable!()
        };
        Ok(Self {})
    }

    fn try_set_reg(
        runner: &mut ProcessorRunner<'_, Self>,
        _vtl: GuestVtl,
        name: HvRegisterName,
        value: HvRegisterValue,
    ) -> Result<bool, super::Error> {
        // Try to set the register in the CPU context, the fastest path. Only
        // VTL-shared registers can be set this way: the CPU context only
        // exposes the last VTL, and if we entered VTL2 on an interrupt,
        // OpenHCL doesn't know what the last VTL is.
        // NOTE: x18 is omitted here as it is managed by the hypervisor.
        let set = match name.into() {
            HvArm64RegisterName::X0
            | HvArm64RegisterName::X1
            | HvArm64RegisterName::X2
            | HvArm64RegisterName::X3
            | HvArm64RegisterName::X4
            | HvArm64RegisterName::X5
            | HvArm64RegisterName::X6
            | HvArm64RegisterName::X7
            | HvArm64RegisterName::X8
            | HvArm64RegisterName::X9
            | HvArm64RegisterName::X10
            | HvArm64RegisterName::X11
            | HvArm64RegisterName::X12
            | HvArm64RegisterName::X13
            | HvArm64RegisterName::X14
            | HvArm64RegisterName::X15
            | HvArm64RegisterName::X16
            | HvArm64RegisterName::X17
            | HvArm64RegisterName::X19
            | HvArm64RegisterName::X20
            | HvArm64RegisterName::X21
            | HvArm64RegisterName::X22
            | HvArm64RegisterName::X23
            | HvArm64RegisterName::X24
            | HvArm64RegisterName::X25
            | HvArm64RegisterName::X26
            | HvArm64RegisterName::X27
            | HvArm64RegisterName::X28
            | HvArm64RegisterName::XFp
            | HvArm64RegisterName::XLr => {
                runner.cpu_context_mut().x[(name.0 - HvArm64RegisterName::X0.0) as usize] =
                    value.as_u64();
                true
            }
            HvArm64RegisterName::X18 => {
                // TODO: handle X18 for VTL1
                runner.cpu_context_mut().x[18] = value.as_u64();
                false
            }
            _ => false,
        };

        Ok(set)
    }

    fn must_flush_regs_on(_runner: &ProcessorRunner<'_, Self>, _name: HvRegisterName) -> bool {
        false
    }

    fn try_get_reg(
        runner: &ProcessorRunner<'_, Self>,
        _vtl: GuestVtl,
        name: HvRegisterName,
    ) -> Result<Option<HvRegisterValue>, super::Error> {
        // Try to get the register from the CPU context, the fastest path.
        // NOTE: x18 is omitted here as it is managed by the hypervisor.
        let value = match name.into() {
            HvArm64RegisterName::X0
            | HvArm64RegisterName::X1
            | HvArm64RegisterName::X2
            | HvArm64RegisterName::X3
            | HvArm64RegisterName::X4
            | HvArm64RegisterName::X5
            | HvArm64RegisterName::X6
            | HvArm64RegisterName::X7
            | HvArm64RegisterName::X8
            | HvArm64RegisterName::X9
            | HvArm64RegisterName::X10
            | HvArm64RegisterName::X11
            | HvArm64RegisterName::X12
            | HvArm64RegisterName::X13
            | HvArm64RegisterName::X14
            | HvArm64RegisterName::X15
            | HvArm64RegisterName::X16
            | HvArm64RegisterName::X17
            | HvArm64RegisterName::X19
            | HvArm64RegisterName::X20
            | HvArm64RegisterName::X21
            | HvArm64RegisterName::X22
            | HvArm64RegisterName::X23
            | HvArm64RegisterName::X24
            | HvArm64RegisterName::X25
            | HvArm64RegisterName::X26
            | HvArm64RegisterName::X27
            | HvArm64RegisterName::X28
            | HvArm64RegisterName::XFp
            | HvArm64RegisterName::XLr => {
                Some(runner.cpu_context().x[(name.0 - HvArm64RegisterName::X0.0) as usize].into())
            }
            _ => None,
        };
        Ok(value)
    }

    fn flush_register_page(_runner: &mut ProcessorRunner<'_, Self>) {}
}
