// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Platform-specific context implementations for AArch64 Hyper-V.
//!

use core::ops::Range;

use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecToken;
use crate::context::VtlPlatformTrait;
use crate::platform::hyperv::ctx::HvTestCtx;
use crate::platform::hyperv::ctx::vtl_transform;
use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;
use hvdef::AlignedU128;
use hvdef::HvRegisterValue;
use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::InitialVpContextArm64;
use memory_range::MemoryRange;

impl VirtualProcessorPlatformTrait<HvTestCtx> for HvTestCtx {
    /// Fetch the content of the specified architectural register from
    /// the current VTL for the executing VP.
    fn get_register(&mut self, reg: u32) -> TmkResult<u128> {
        let reg = hvdef::HvArm64RegisterName(reg);
        let val = self.hvcall.get_register(reg.into(), None)?.as_u128();
        Ok(val)
    }

    /// Set the architecture specific register identified by `reg`.
    fn set_register(&mut self, reg: u32, val: u128) -> TmkResult<()> {
        let reg = hvdef::HvArm64RegisterName(reg);
        let value = HvRegisterValue::from(val);
        self.hvcall.set_register(reg.into(), value, None)?;
        Ok(())
    }

    fn get_vp_count(&self) -> TmkResult<u32> {
        unimplemented!();
    }

    fn queue_command_vp(&mut self, _cmd: VpExecToken<HvTestCtx>) -> TmkResult<()> {
        unimplemented!();
    }

    fn start_on_vp(&mut self, _cmd: VpExecToken<HvTestCtx>) -> TmkResult<()> {
        unimplemented!();
    }

    /// Start the given VP in the current VTL using a freshly captured
    /// context.
    fn start_running_vp_with_default_context(
        &mut self,
        cmd: VpExecToken<HvTestCtx>,
    ) -> TmkResult<()> {
        let (vp_index, vtl, _cmd) = cmd.get();
        let vp_ctx = self.get_default_context(vtl)?;
        self.hvcall
            .start_virtual_processor(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    /// Return the index of the VP that is currently executing this code.
    fn get_current_vp(&self) -> TmkResult<u32> {
        Ok(self.my_vp_idx)
    }

    fn set_register_vtl(&mut self, reg: u32, value: u128, vtl: Vtl) -> TmkResult<()> {
        let reg = hvdef::HvArm64RegisterName(reg);
        let value = HvRegisterValue::from(value);
        self.hvcall
            .set_register(reg.into(), value, Some(vtl_transform(vtl)))?;
        Ok(())
    }

    fn get_register_vtl(&mut self, reg: u32, vtl: Vtl) -> TmkResult<u128> {
        let reg = hvdef::HvArm64RegisterName(reg);
        let val = self
            .hvcall
            .get_register(reg.into(), Some(vtl_transform(vtl)))?
            .as_u128();
        Ok(val)
    }
}

impl VtlPlatformTrait for HvTestCtx {
    /// Apply VTL protections to the supplied GPA range so that only the
    /// provided VTL can access it.
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .apply_vtl_protections(MemoryRange::new(range), vtl)?;
        Ok(())
    }

    /// Enable the specified VTL on a VP and seed it with a default
    /// context captured from the current execution environment.
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()> {
        let vp_ctx = self.get_default_context(vtl)?;
        self.hvcall.enable_vp_vtl(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    /// Return the VTL in which the current code is running.
    fn get_current_vtl(&self) -> TmkResult<Vtl> {
        Ok(self.my_vtl)
    }

    /// Enable VTL support for the entire partition.
    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .enable_partition_vtl(hvdef::HV_PARTITION_ID_SELF, vtl)?;
        log::info!("enabled {:?} for the partition.", vtl);
        Ok(())
    }

    /// Turn on VTL protections for the currently running VTL.
    fn setup_vtl_protection(&mut self) -> TmkResult<()> {
        self.hvcall.enable_vtl_protection(HvInputVtl::CURRENT_VTL)?;
        log::info!("enabled vtl protections for the partition.");
        Ok(())
    }

    /// Switch execution from the current (low) VTL to the next higher
    /// one (`vtl_call`).
    fn switch_to_high_vtl(&mut self) {}

    /// Return from a high VTL back to the low VTL (`vtl_return`).
    fn switch_to_low_vtl(&mut self) {}

    fn set_vp_register_with_vtl(
        &mut self,
        register_index: u32,
        value: u64,
        vtl: Vtl,
    ) -> TmkResult<()> {
        let vtl = vtl_transform(vtl);
        let value = AlignedU128::from(value);
        let reg_value = HvRegisterValue(value);
        self.hvcall
            .set_register(hvdef::HvRegisterName(register_index), reg_value, Some(vtl))
            .map_err(|e| e.into())
    }

    fn get_vp_register_with_vtl(&mut self, register_index: u32, vtl: Vtl) -> TmkResult<u64> {
        let vtl = vtl_transform(vtl);
        self.hvcall
            .get_register(hvdef::HvRegisterName(register_index), Some(vtl))
            .map(|v| v.as_u64())
            .map_err(|e| e.into())
    }
}

impl HvTestCtx {
    fn get_default_context(&mut self, _vtl: Vtl) -> Result<InitialVpContextArm64, TmkError> {
        unimplemented!("aarch64 not implemented");
    }

    pub(crate) fn get_vp_idx() -> u32 {
        unimplemented!()
    }
}
