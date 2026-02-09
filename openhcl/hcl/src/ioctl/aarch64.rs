// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for non-hardware-isolated ARM64 partitions.

use super::Hcl;
use super::HclVp;
use super::NoRunner;
use super::ProcessorRunner;
use super::TranslateGvaToGpaError;
use super::TranslateResult;
use crate::GuestVtl;
use crate::protocol::hcl_cpu_context_aarch64;
use hvdef::HV_PARTITION_ID_SELF;
use hvdef::HV_VP_INDEX_SELF;
use hvdef::HvAarch64RegisterPage;
use hvdef::HvArm64RegisterName;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HypercallCode;
use sidecar_client::SidecarVp;
use std::cell::UnsafeCell;
use thiserror::Error;
use zerocopy::FromZeros;

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
pub struct MshvArm64<'a> {
    reg_page: Option<&'a UnsafeCell<HvAarch64RegisterPage>>,
}

impl<'a> ProcessorRunner<'a, MshvArm64<'a>> {
    fn reg_page(&self) -> Option<&HvAarch64RegisterPage> {
        // SAFETY: the register page will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        let reg_page = unsafe { &*self.state.reg_page?.get() };
        if reg_page.is_valid != 0 {
            Some(reg_page)
        } else {
            None
        }
    }

    fn reg_page_mut(&mut self) -> Option<&mut HvAarch64RegisterPage> {
        // SAFETY: the register page will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        let reg_page = unsafe { &mut *self.state.reg_page?.get() };
        if reg_page.is_valid != 0 {
            Some(reg_page)
        } else {
            None
        }
    }

    /// Returns a reference to the current VTL's CPU context.
    pub fn cpu_context(&self) -> &hcl_cpu_context_aarch64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &*(&raw mut (*self.run.get()).context).cast() }
    }

    /// Returns a mutable reference to the current VTL's CPU context.
    pub fn cpu_context_mut(&mut self) -> &mut hcl_cpu_context_aarch64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &mut *(&raw mut (*self.run.get()).context).cast() }
    }

    /// Translate the following gva to a gpa page.
    ///
    /// The caller must ensure `control_flags.input_vtl()` is set to a specific
    /// VTL.
    pub fn translate_gva_to_gpa(
        &self,
        gva: u64,
        control_flags: hvdef::hypercall::TranslateGvaControlFlagsArm64,
    ) -> Result<Result<TranslateResult, TranslateErrorAarch64>, TranslateGvaToGpaError> {
        use hvdef::hypercall;

        assert!(
            control_flags.input_vtl().use_target_vtl(),
            "did not specify a target VTL"
        );

        let gvn = gva >> hvdef::HV_PAGE_SHIFT;
        let header = hypercall::TranslateVirtualAddressArm64 {
            partition_id: HV_PARTITION_ID_SELF,
            vp_index: HV_VP_INDEX_SELF,
            reserved: 0,
            control_flags,
            gva_page: gvn,
        };

        let mut output: hypercall::TranslateVirtualAddressExOutputArm64 = FromZeros::new_zeroed();

        // SAFETY: The input header and slice are the correct types for this hypercall.
        //         The hypercall output is validated right after the hypercall is issued.
        let status = unsafe {
            self.hcl
                .mshv_hvcall
                .hvcall(
                    HypercallCode::HvCallTranslateVirtualAddressEx,
                    &header,
                    &mut output,
                )
                .expect("translate can never fail")
        };

        status
            .result()
            .map_err(|hv_error| TranslateGvaToGpaError::Hypervisor { gva, hv_error })?;

        // Note: WHP doesn't currently support TranslateVirtualAddressEx, so overlay_page, cache_type,
        // event_info aren't trustworthy values if the results came from WHP.
        match output.translation_result.result.result_code() {
            c if c == hypercall::TranslateGvaResultCode::SUCCESS.0 => Ok(Ok(TranslateResult {
                gpa_page: output.gpa_page,
                overlay_page: output.translation_result.result.overlay_page(),
            })),
            x => Ok(Err(TranslateErrorAarch64 { code: x })),
        }
    }
}

impl<'a> super::BackingPrivate<'a> for MshvArm64<'a> {
    fn new(vp: &'a HclVp, sidecar: Option<&SidecarVp<'_>>, _hcl: &Hcl) -> Result<Self, NoRunner> {
        assert!(sidecar.is_none());
        let super::BackingState::MshvAarch64 { reg_page } = &vp.backing else {
            unreachable!()
        };
        Ok(Self {
            reg_page: reg_page.as_ref().map(|x| x.as_ref()),
        })
    }

    fn try_set_reg(
        runner: &mut ProcessorRunner<'a, Self>,
        vtl: GuestVtl,
        name: HvRegisterName,
        value: HvRegisterValue,
    ) -> bool {
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
        if set {
            return true;
        }

        if let Some(reg_page) = runner.reg_page_mut() {
            if reg_page.vtl == vtl as u8 {
                let set = match name.into() {
                    HvArm64RegisterName::XPc => {
                        reg_page.pc = value.as_u64();
                        reg_page.dirty.set_instruction_pointer(true);
                        true
                    }
                    HvArm64RegisterName::Cpsr => {
                        reg_page.cpsr = value.as_u64();
                        reg_page.dirty.set_processor_state(true);
                        true
                    }
                    HvArm64RegisterName::SctlrEl1 => {
                        reg_page.sctlr_el1 = value.as_u64();
                        reg_page.dirty.set_control_registers(true);
                        true
                    }
                    HvArm64RegisterName::TcrEl1 => {
                        reg_page.tcr_el1 = value.as_u64();
                        reg_page.dirty.set_control_registers(true);
                        true
                    }
                    _ => false,
                };
                if set {
                    return true;
                }
            }
        };
        false
    }

    fn must_flush_regs_on(_runner: &ProcessorRunner<'a, Self>, _name: HvRegisterName) -> bool {
        false
    }

    fn try_get_reg(
        runner: &ProcessorRunner<'a, Self>,
        vtl: GuestVtl,
        name: HvRegisterName,
    ) -> Option<HvRegisterValue> {
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
        if value.is_some() {
            return value;
        }

        if let Some(reg_page) = runner.reg_page() {
            if reg_page.vtl == vtl as u8 {
                let value = match name.into() {
                    HvArm64RegisterName::XPc => Some(HvRegisterValue((reg_page.pc).into())),
                    HvArm64RegisterName::Cpsr => Some(HvRegisterValue((reg_page.cpsr).into())),
                    HvArm64RegisterName::SctlrEl1 => {
                        Some(HvRegisterValue((reg_page.sctlr_el1).into()))
                    }
                    HvArm64RegisterName::TcrEl1 => Some(HvRegisterValue((reg_page.tcr_el1).into())),
                    _ => None,
                };
                if value.is_some() {
                    return value;
                }
            }
        };
        None
    }

    fn flush_register_page(runner: &mut ProcessorRunner<'a, Self>) {
        let Some(reg_page) = runner.reg_page_mut() else {
            return;
        };

        // Collect any dirty registers.
        let mut regs: Vec<(HvArm64RegisterName, HvRegisterValue)> = Vec::new();
        if reg_page.dirty.instruction_pointer() {
            regs.push((HvArm64RegisterName::XPc, reg_page.pc.into()));
        }
        if reg_page.dirty.processor_state() {
            regs.push((HvArm64RegisterName::Cpsr, reg_page.cpsr.into()));
        }
        if reg_page.dirty.control_registers() {
            regs.push((HvArm64RegisterName::SctlrEl1, reg_page.sctlr_el1.into()));
            regs.push((HvArm64RegisterName::TcrEl1, reg_page.tcr_el1.into()));
        }

        // Disable the reg page so future writes do not use it (until the state
        // is reset at the next VTL transition).
        reg_page.is_valid = 0;
        reg_page.dirty = 0.into();

        // Set the registers now that the register page is marked invalid.
        if let Err(err) = runner.set_vp_registers(GuestVtl::Vtl0, regs.as_slice()) {
            panic!(
                "Failed to flush register page: {}",
                &err as &dyn std::error::Error
            );
        }
    }
}
