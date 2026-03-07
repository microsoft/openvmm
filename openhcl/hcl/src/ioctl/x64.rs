// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for non-hardware-isolated X64 partitions.

use super::BackingState;
use super::Error;
use super::GuestVtl;
use super::Hcl;
use super::HclVp;
use super::NoRunner;
use super::ProcessorRunner;
use super::TranslateGvaToGpaError;
use super::TranslateResult;
use super::private::BackingPrivate;
use crate::protocol::hcl_cpu_context_x64;
use hvdef::HV_PARTITION_ID_SELF;
use hvdef::HV_VP_INDEX_SELF;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use hvdef::HvX64RegisterPage;
use hvdef::HypercallCode;
use sidecar_client::SidecarVp;
use std::cell::UnsafeCell;
use zerocopy::FromZeros;

/// Result when the translate gva hypercall returns a code indicating
/// the translation was unsuccessful.
#[derive(Error, Debug)]
#[error("translate gva to gpa returned non-successful code {code:?}")]
pub struct TranslateErrorX64 {
    /// The code returned by the translate gva hypercall.
    pub code: u32,
    /// The event to inject.
    pub event_info: hvdef::HvX64PendingEvent,
}

/// Result when the intercepted vtl is invalid.
#[derive(Error, Debug)]
#[expect(missing_docs)]
pub enum RegisterPageVtlError {
    #[error("no register page")]
    NoRegisterPage,
    #[error("invalid guest vtl {0}")]
    InvalidVtl(u8),
}

/// Runner backing for non-hardware-isolated X64 partitions.
pub struct MshvX64<'a> {
    reg_page: Option<&'a UnsafeCell<HvX64RegisterPage>>,
    cpu_context: &'a UnsafeCell<hcl_cpu_context_x64>,
}

impl<'a> ProcessorRunner<'a, MshvX64<'a>> {
    fn reg_page(&self) -> Option<&HvX64RegisterPage> {
        // SAFETY: the register page will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        let reg_page = unsafe { &*self.state.reg_page?.get() };
        if reg_page.is_valid != 0 {
            Some(reg_page)
        } else {
            None
        }
    }

    fn reg_page_mut(&mut self) -> Option<&mut HvX64RegisterPage> {
        // SAFETY: the register page will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        let reg_page = unsafe { &mut *self.state.reg_page?.get() };
        if reg_page.is_valid != 0 {
            Some(reg_page)
        } else {
            None
        }
    }

    /// Returns the last VTL according to the register page.
    pub fn reg_page_vtl(&self) -> Result<GuestVtl, RegisterPageVtlError> {
        // Note: if available, the register page is only valid if VTL 2 is
        // handling an intercept.
        let vtl = self
            .reg_page()
            .ok_or(RegisterPageVtlError::NoRegisterPage)?
            .vtl;
        vtl.try_into()
            .map_err(|_| RegisterPageVtlError::InvalidVtl(vtl))
    }

    /// Returns a reference to the current VTL's CPU context.
    pub fn cpu_context(&self) -> &hcl_cpu_context_x64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // kernel while this VP is in user mode.
        unsafe { &*self.state.cpu_context.get() }
    }

    /// Returns a mutable reference to the current VTL's CPU context.
    pub fn cpu_context_mut(&mut self) -> &mut hcl_cpu_context_x64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // kernel while this VP is in user mode.
        unsafe { &mut *self.state.cpu_context.get() }
    }

    /// Translate the following gva to a gpa page in the context of the current
    /// VP.
    ///
    /// The caller must ensure `control_flags.input_vtl()` is set to a specific
    /// VTL.
    pub fn translate_gva_to_gpa(
        &mut self,
        gva: u64,
        control_flags: hvdef::hypercall::TranslateGvaControlFlagsX64,
    ) -> Result<Result<TranslateResult, TranslateErrorX64>, TranslateGvaToGpaError> {
        use hvdef::hypercall;

        assert!(
            control_flags.input_vtl().use_target_vtl(),
            "did not specify a target VTL"
        );

        let gvn = gva >> hvdef::HV_PAGE_SHIFT;
        let output = if let Some(sidecar) = &mut self.sidecar {
            sidecar
                .translate_gva(gvn, control_flags)
                .map_err(|err| TranslateGvaToGpaError::Sidecar { error: err, gva })?
        } else {
            let header = hypercall::TranslateVirtualAddressX64 {
                partition_id: HV_PARTITION_ID_SELF,
                vp_index: HV_VP_INDEX_SELF,
                reserved: 0,
                control_flags,
                gva_page: gvn,
            };

            let mut output: hypercall::TranslateVirtualAddressExOutputX64 = FromZeros::new_zeroed();

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

            output
        };

        // Note: WHP doesn't currently support TranslateVirtualAddressEx, so overlay_page, cache_type,
        // event_info aren't trustworthy values if the results came from WHP.
        match output.translation_result.result.result_code() {
            c if c == hypercall::TranslateGvaResultCode::SUCCESS.0 => Ok(Ok(TranslateResult {
                gpa_page: output.gpa_page,
                overlay_page: output.translation_result.result.overlay_page(),
            })),
            x => Ok(Err(TranslateErrorX64 {
                code: x,
                event_info: output.translation_result.event_info,
            })),
        }
    }
}

impl<'a> BackingPrivate<'a> for MshvX64<'a> {
    fn new(vp: &'a HclVp, sidecar: Option<&SidecarVp<'a>>, _hcl: &Hcl) -> Result<Self, NoRunner> {
        let BackingState::MshvX64 { reg_page } = &vp.backing else {
            return Err(NoRunner::MismatchedIsolation);
        };

        // SAFETY: The run page and register page, whether provided locally
        // or by sidecar, are guaranteed to be mapped and valid.
        unsafe {
            let this = if let Some(sidecar) = sidecar {
                // Sidecar always provides a register page, but it may not actually
                // be mapped with the hypervisor. Use the sidecar's register page
                // only if the mshv_vtl driver thinks there should be one.
                Self {
                    reg_page: reg_page.is_some().then(|| &*sidecar.register_page().cast()),
                    cpu_context: &*sidecar.cpu_context().cast(),
                }
            } else {
                Self {
                    reg_page: reg_page.as_ref().map(|x| x.as_ref()),
                    cpu_context: &*(&raw mut (*vp.run.as_ptr()).context).cast(),
                }
            };

            Ok(this)
        }
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
        let name = name.into();
        let set = match name {
            HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rbp
            | HvX64RegisterName::Rsi
            | HvX64RegisterName::Rdi
            | HvX64RegisterName::R8
            | HvX64RegisterName::R9
            | HvX64RegisterName::R10
            | HvX64RegisterName::R11
            | HvX64RegisterName::R12
            | HvX64RegisterName::R13
            | HvX64RegisterName::R14
            | HvX64RegisterName::R15 => {
                runner.cpu_context_mut().gps[(name.0 - HvX64RegisterName::Rax.0) as usize] =
                    value.as_u64();
                true
            }

            HvX64RegisterName::Cr2 => {
                // CR2 is stored in the RSP slot.
                runner.cpu_context_mut().gps
                    [(HvX64RegisterName::Rsp.0 - HvX64RegisterName::Rax.0) as usize] =
                    value.as_u64();
                true
            }

            HvX64RegisterName::Xmm0
            | HvX64RegisterName::Xmm1
            | HvX64RegisterName::Xmm2
            | HvX64RegisterName::Xmm3
            | HvX64RegisterName::Xmm4
            | HvX64RegisterName::Xmm5 => {
                runner.cpu_context_mut().fx_state.xmm
                    [(name.0 - HvX64RegisterName::Xmm0.0) as usize] = value.as_u128().to_ne_bytes();
                true
            }
            _ => false,
        };
        if set {
            return true;
        }

        if let Some(reg_page) = runner.reg_page_mut() {
            if reg_page.vtl == vtl as u8 {
                let set = match name {
                    HvX64RegisterName::Rsp => {
                        reg_page.gp_registers[(name.0 - HvX64RegisterName::Rax.0) as usize] =
                            value.as_u64();
                        reg_page.dirty.set_general_purpose(true);
                        true
                    }
                    HvX64RegisterName::Rip => {
                        reg_page.rip = value.as_u64();
                        reg_page.dirty.set_instruction_pointer(true);
                        true
                    }
                    HvX64RegisterName::Rflags => {
                        reg_page.rflags = value.as_u64();
                        reg_page.dirty.set_flags(true);
                        true
                    }
                    HvX64RegisterName::Es
                    | HvX64RegisterName::Cs
                    | HvX64RegisterName::Ss
                    | HvX64RegisterName::Ds
                    | HvX64RegisterName::Fs
                    | HvX64RegisterName::Gs => {
                        reg_page.segment[(name.0 - HvX64RegisterName::Es.0) as usize] =
                            value.as_u128();
                        reg_page.dirty.set_segments(true);
                        true
                    }

                    // Skip unnecessary register updates.
                    HvX64RegisterName::Cr0 => reg_page.cr0 == value.as_u64(),
                    HvX64RegisterName::Cr3 => reg_page.cr3 == value.as_u64(),
                    HvX64RegisterName::Cr4 => reg_page.cr4 == value.as_u64(),
                    HvX64RegisterName::Cr8 => reg_page.cr8 == value.as_u64(),
                    HvX64RegisterName::Efer => reg_page.efer == value.as_u64(),
                    HvX64RegisterName::Dr7 => reg_page.dr7 == value.as_u64(),
                    _ => false,
                };
                if set {
                    return true;
                }
            }
        }
        false
    }

    fn must_flush_regs_on(runner: &ProcessorRunner<'a, Self>, name: HvRegisterName) -> bool {
        // Updating rflags must be ordered with other registers in a batch,
        // since it may affect the validity other interrupt-related registers.
        matches!(HvX64RegisterName::from(name), HvX64RegisterName::Rflags)
            && runner.reg_page().is_some()
    }

    fn try_get_reg(
        runner: &ProcessorRunner<'a, Self>,
        vtl: GuestVtl,
        name: HvRegisterName,
    ) -> Option<HvRegisterValue> {
        let name = name.into();

        let value = match name {
            HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rbp
            | HvX64RegisterName::Rsi
            | HvX64RegisterName::Rdi
            | HvX64RegisterName::R8
            | HvX64RegisterName::R9
            | HvX64RegisterName::R10
            | HvX64RegisterName::R11
            | HvX64RegisterName::R12
            | HvX64RegisterName::R13
            | HvX64RegisterName::R14
            | HvX64RegisterName::R15 => {
                Some(runner.cpu_context().gps[(name.0 - HvX64RegisterName::Rax.0) as usize].into())
            }

            HvX64RegisterName::Cr2 => {
                // CR2 is stored in the RSP slot.
                Some(
                    runner.cpu_context().gps
                        [(HvX64RegisterName::Rsp.0 - HvX64RegisterName::Rax.0) as usize]
                        .into(),
                )
            }

            HvX64RegisterName::Xmm0
            | HvX64RegisterName::Xmm1
            | HvX64RegisterName::Xmm2
            | HvX64RegisterName::Xmm3
            | HvX64RegisterName::Xmm4
            | HvX64RegisterName::Xmm5 => Some(
                u128::from_ne_bytes(
                    runner.cpu_context().fx_state.xmm
                        [(name.0 - HvX64RegisterName::Xmm0.0) as usize],
                )
                .into(),
            ),
            _ => None,
        };
        if value.is_some() {
            return value;
        }

        if let Some(reg_page) = runner.reg_page() {
            if reg_page.vtl == vtl as u8 {
                let value = match name {
                    HvX64RegisterName::Rsp => Some(HvRegisterValue(
                        reg_page.gp_registers[(name.0 - HvX64RegisterName::Rax.0) as usize].into(),
                    )),
                    HvX64RegisterName::Rip => Some(HvRegisterValue((reg_page.rip).into())),
                    HvX64RegisterName::Rflags => Some(HvRegisterValue((reg_page.rflags).into())),
                    HvX64RegisterName::Es
                    | HvX64RegisterName::Cs
                    | HvX64RegisterName::Ss
                    | HvX64RegisterName::Ds
                    | HvX64RegisterName::Fs
                    | HvX64RegisterName::Gs => Some(HvRegisterValue(
                        reg_page.segment[(name.0 - HvX64RegisterName::Es.0) as usize].into(),
                    )),
                    HvX64RegisterName::Cr0 => Some(HvRegisterValue((reg_page.cr0).into())),
                    HvX64RegisterName::Cr3 => Some(HvRegisterValue((reg_page.cr3).into())),
                    HvX64RegisterName::Cr4 => Some(HvRegisterValue((reg_page.cr4).into())),
                    HvX64RegisterName::Cr8 => Some(HvRegisterValue((reg_page.cr8).into())),
                    HvX64RegisterName::Efer => Some(HvRegisterValue((reg_page.efer).into())),
                    HvX64RegisterName::Dr7 => Some(HvRegisterValue((reg_page.dr7).into())),
                    HvX64RegisterName::InstructionEmulationHints => Some(HvRegisterValue(
                        (u64::from(reg_page.instruction_emulation_hints)).into(),
                    )),
                    HvX64RegisterName::PendingInterruption => {
                        Some(u64::from(reg_page.pending_interruption).into())
                    }
                    HvX64RegisterName::InterruptState => {
                        Some(u64::from(reg_page.interrupt_state).into())
                    }
                    _ => None,
                };
                if value.is_some() {
                    return value;
                }
            }
        }
        None
    }

    fn flush_register_page(runner: &mut ProcessorRunner<'a, Self>) {
        let Some(reg_page) = runner.reg_page_mut() else {
            return;
        };

        // Collect any dirty registers.
        let mut regs: Vec<(HvX64RegisterName, HvRegisterValue)> = Vec::new();
        if reg_page.dirty.instruction_pointer() {
            regs.push((HvX64RegisterName::Rip, reg_page.rip.into()));
        }
        if reg_page.dirty.general_purpose() {
            regs.push((
                HvX64RegisterName::Rsp,
                reg_page.gp_registers
                    [(HvX64RegisterName::Rsp.0 - HvX64RegisterName::Rax.0) as usize]
                    .into(),
            ));
        }
        if reg_page.dirty.flags() {
            regs.push((HvX64RegisterName::Rflags, reg_page.rflags.into()));
        }
        if reg_page.dirty.segments() {
            let segment_regs = reg_page
                .segment
                .iter()
                .copied()
                .enumerate()
                .map(|(i, val)| {
                    (
                        HvX64RegisterName::from(HvRegisterName(HvX64RegisterName::Es.0 + i as u32)),
                        HvRegisterValue::from(val),
                    )
                });
            regs.extend(segment_regs);
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
