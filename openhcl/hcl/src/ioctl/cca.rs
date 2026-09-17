// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for CCA partitions.

use std::os::fd::AsRawFd;

use super::Hcl;
use super::HclVp;
use super::MshvVtl;
use super::NoRunner;
use super::ProcessorRunner;
use crate::GuestVtl;
use crate::ioctl::Error;
use crate::ioctl::GetRegError;
use crate::ioctl::HvError;
use crate::ioctl::SetRegError;
use crate::ioctl::ioctls::hcl_realm_config;
use crate::ioctl::ioctls::hcl_rsi_ipa_state_read;
use crate::ioctl::ioctls::hcl_rsi_set_mem_perm;
use crate::ioctl::ioctls::hcl_rsi_sysreg_read;
use crate::ioctl::ioctls::hcl_rsi_sysreg_write;
use aarch64defs::SystemReg;
use aarch64defs::rsi::RSI_PLANE_ENTER_FLAGS_TRAP_SIMD;
use aarch64defs::rsi::RSI_PLANE_GIC_NUM_LRS;
use aarch64defs::rsi::RSI_PLANE_NR_GPRS;
use aarch64defs::rsi::cca_rsi_plane_entry;
use aarch64defs::rsi::cca_rsi_plane_exit;
use aarch64defs::rsi::cca_rsi_plane_run;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvArm64RegisterName;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use memory_range::MemoryRange;
use sidecar_client::SidecarVp;
use user_driver::memory::MemoryBlock;

#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum GetIpaStateError {
    #[error("RSI IPA state read ioctl failed")]
    Ioctl(#[source] nix::Error),
}

/// CCA: Structure mirroring the data returned by RMM in the RSI_REALM_CONFIG call.
#[repr(C, align(0x1000))]
#[derive(Clone, Copy, Default)]
#[expect(missing_docs)]
pub struct mshv_realm_config {
    pub ipa_width: u64,
    pub algorithm: u64,
    pub num_aux_planes: u64,
    pub gicv3_vtr: u64,
}

/// CCA: Structure mirroring the data taken by RMM in the RSI_PLANE_SYSREG_WRITE.
/// `vtl` is converted into plane number in kernel driver.
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[expect(missing_docs)]
pub struct mshv_rsi_sysreg_rw {
    pub vtl: u8,
    pub _pad: [u8; 7],
    pub sysreg: u64,
    pub value: u64,
}

/// CCA: Structure mirroring the data taken by RMM in the RSI_SET_MEM_PERM.
/// NOTE: we hand over the plane number here, we should probably stay consistent with
///       `sysreg_write`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[expect(missing_docs)]
pub struct mshv_rsi_set_mem_perm {
    pub plane: u8,
    pub _pad: [u8; 7],
    pub base_addr: u64,
    pub top_addr: u64,
}

/// CCA: Structure used by the hcl_rsi_ipa_state_read ioctl.
/// Caller sets fipa to the faulting IPA. On return the state
/// contains the corresponding RIPAS state.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct mshv_rsi_get_ipa_state {
    /// Faulting ipa to have its state queried
    pub fipa: u64,
    /// RIPAS state returned for fipa
    pub state: u64,
}

/// SystemReg is following encoding used by MSR/MRS which is different with
/// the encoding RSI is using. The latter doesn't left shift the register
/// number.
const fn encode_rsi_sysreg(sysreg: SystemReg) -> u64 {
    ((sysreg.0.op0() as u64) << 14)
        | ((sysreg.0.op1() as u64) << 11)
        | ((sysreg.0.crn() as u64) << 7)
        | ((sysreg.0.crm() as u64) << 3)
        | (sysreg.0.op2() as u64)
}

/// Runner backing for CCA partitions.
pub struct Cca {
    plane_run: MemoryBlock,
}

impl Cca {
    /// Create new CCA runner backing.
    pub fn new(plane_run: &MemoryBlock) -> Self {
        assert_eq!(plane_run.offset_in_page(), 0);
        assert!(plane_run.len() >= size_of::<cca_rsi_plane_run>());

        Self {
            plane_run: plane_run.clone(),
        }
    }

    fn plane_run_ref(&self) -> &cca_rsi_plane_run {
        // SAFETY: the DMA allocation remains mapped for the lifetime of the backing
        // and is page-aligned, so it can be viewed as a `cca_rsi_plane_run`. Also,
        // 'new' validates that the allocation size is >= sizeof cca_rsi_plane_run.
        unsafe { &*self.plane_run.base().cast::<cca_rsi_plane_run>() }
    }

    fn plane_run_mut(&mut self) -> &mut cca_rsi_plane_run {
        // SAFETY: the DMA allocation remains mapped for the lifetime of the backing
        // and `&mut self` guarantees exclusive access to the mapped page contents.
        unsafe { &mut *self.plane_run.base().cast_mut().cast::<cca_rsi_plane_run>() }
    }

    fn plane_run_phys(&self) -> u64 {
        self.plane_run.pfns()[0] * HV_PAGE_SIZE
    }
}

impl ProcessorRunner<'_, Cca> {
    /// Returns a reference to the current VTL's CPU context.
    pub fn cpu_context(&self) -> &u64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &*(&raw mut (*self.run.get()).context).cast() }
    }

    /// Returns a mutable reference to the current VTL's CPU context.
    pub fn cpu_context_mut(&mut self) -> &mut u64 {
        // SAFETY: the cpu context will not be concurrently accessed by the
        // hypervisor while this VP is in VTL2.
        unsafe { &mut *(&raw mut (*self.run.get()).context).cast() }
    }

    /// Returns a mutable reference to the current VTL's CCA RSI plane run structure.
    pub fn cca_rsi_plane_run_mut(&mut self) -> &mut cca_rsi_plane_run {
        self.state.plane_run_mut()
    }

    /// Returns a mutable reference to the current VTL's plane entry structure.
    pub fn cca_rsi_plane_entry(&mut self) -> &mut cca_rsi_plane_entry {
        &mut self.state.plane_run_mut().entry
    }

    /// Returns a mutable reference to the current VTL's plane exit structure.
    pub fn cca_rsi_plane_exit(&self) -> &cca_rsi_plane_exit {
        &self.state.plane_run_ref().exit
    }

    /// Set the value of the plane entry flags.
    pub fn cca_set_entry_flags(&mut self, value: u64) {
        self.cca_rsi_plane_entry().flags = value;
    }

    /// Set the value of the plane entry PC.
    pub fn cca_set_entry_pc(&mut self, value: u64) {
        self.cca_rsi_plane_entry().pc = value;
    }

    /// Set the value of the plane entry GPRs.
    pub fn cca_set_entry_gprs(&mut self, values: [u64; RSI_PLANE_NR_GPRS]) {
        self.cca_rsi_plane_entry().gprs = values;
    }

    /// Set the value of the plane entry gicv3_hcr register.
    pub fn cca_set_entry_gicv3_hcr(&mut self, value: u64) {
        self.cca_rsi_plane_entry().gicv3_hcr = value;
    }

    /// Set the value of the plane entry GIC v3 LRs.
    pub fn cca_set_entry_gicv3_lrs(&mut self, values: [u64; RSI_PLANE_GIC_NUM_LRS]) {
        self.cca_rsi_plane_entry().gicv3_lrs = values;
    }

    /// Set the value of a single plane entry GPR.
    fn cca_set_entry_gpr(&mut self, register: usize, value: u64) {
        assert!(register < RSI_PLANE_NR_GPRS);
        self.cca_rsi_plane_entry().gprs[register] = value;
    }

    /// Get the value of a single plane entry GPR.
    fn cca_get_entry_gpr(&self, register: usize) -> u64 {
        assert!(register < RSI_PLANE_NR_GPRS);
        self.cca_rsi_plane_exit().gprs[register]
    }

    /// Flush the given value for a system register to the RMM.
    pub fn cca_sysreg_write(
        &mut self,
        vtl: GuestVtl,
        name: SystemReg,
        value: u64,
    ) -> Result<(), SetRegError> {
        self.hcl
            .rsi_sysreg_write(vtl, encode_rsi_sysreg(name), value)
    }

    /// Read the value of a system register from the RMM.
    pub fn cca_sysreg_read(
        &mut self,
        vtl: GuestVtl,
        name: SystemReg,
        value: &mut u64,
    ) -> Result<(), GetRegError> {
        self.hcl
            .rsi_sysreg_read(vtl, encode_rsi_sysreg(name), value)
    }

    /// Get the ipa ripas state from the RMM
    pub fn cca_ipa_state_read(&self, fipa: u64) -> Result<Option<u64>, GetIpaStateError> {
        self.hcl.rsi_get_ipa_state(fipa)
    }

    /// Update the address of the `plane_run` structure in `mshv_vtl_run.context`.
    pub fn cca_set_plane_enter(&mut self) {
        // SAFETY: the run page is exclusively accessed through `&mut self` while
        // this VP is in VTL2, and the CCA runner uses `context` as a u64
        // physical address slot for the plane run page.
        let plane_run: &mut u64 = unsafe { &mut *(&raw mut (*self.run.get()).context).cast() };
        *plane_run = self.state.plane_run_phys();
    }

    /// Set flag to enable trapping of SIMD operations in the lower VTL.
    pub fn cca_plane_trap_simd(&mut self) {
        let plane_run: &mut cca_rsi_plane_run = self.state.plane_run_mut();
        plane_run.entry.flags |= RSI_PLANE_ENTER_FLAGS_TRAP_SIMD;
    }

    /// Unset flag that enables trapping of SIMD operations in lower VTL
    /// (i.e., SIMD operations are not trapped).
    pub fn cca_plane_no_trap_simd(&mut self) {
        let plane_run: &mut cca_rsi_plane_run = self.state.plane_run_mut();
        plane_run.entry.flags &= !RSI_PLANE_ENTER_FLAGS_TRAP_SIMD;
    }

    /// Set the default value for PSTATE for the lower VTL.
    pub fn cca_set_default_pstate(&mut self) {
        // SPSR_EL2_MODE_EL1h | SPSR_EL2_nRW_AARCH64 | SPSR_EL2_F_BIT | SPSR_EL2_I_BIT | SPSR_EL2_A_BIT | SPSR_EL2_D_BIT
        self.cca_rsi_plane_entry().pstate = 0x3c5;
    }
}

// TODO CCA: this implementation is lifted from the aarch64 VBS implementation
// and might need more work to make it CCA-aligned.
impl<'a> super::BackingPrivate<'a> for Cca {
    fn new(vp: &HclVp, sidecar: Option<&SidecarVp<'_>>, _hcl: &Hcl) -> Result<Self, NoRunner> {
        assert!(sidecar.is_none());
        let super::BackingState::Cca { plane_run } = &vp.backing else {
            unreachable!()
        };
        let cca = Cca::new(plane_run);

        Ok(cca)
    }

    fn try_set_reg(
        runner: &mut ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        name: HvRegisterName,
        value: HvRegisterValue,
    ) -> bool {
        // Try to set the register in the CPU context, the fastest path. Only
        // VTL-shared registers can be set this way: the CPU context only
        // exposes the last VTL, and if we entered VTL2 on an interrupt,
        // OpenHCL doesn't know what the last VTL is.
        match name.into() {
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
            | HvArm64RegisterName::X18
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
                runner.cca_set_entry_gpr(
                    (name.0 - HvArm64RegisterName::X0.0) as usize,
                    value.as_u64(),
                );
                true
            }
            _ => false,
        }
    }

    fn must_flush_regs_on(_runner: &ProcessorRunner<'a, Self>, _name: HvRegisterName) -> bool {
        false
    }

    fn try_get_reg(
        runner: &ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        name: HvRegisterName,
    ) -> Option<HvRegisterValue> {
        // Try to get the register from the CPU context, the fastest path.
        // NOTE: for VBS x18 is omitted here as it is managed by the hypervisor,
        //       do we need to do the same here?
        match name.into() {
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
            | HvArm64RegisterName::X18
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
            | HvArm64RegisterName::XLr => Some(
                runner
                    .cca_get_entry_gpr((name.0 - HvArm64RegisterName::X0.0) as usize)
                    .into(),
            ),
            _ => None,
        }
    }

    fn flush_register_page(_runner: &mut ProcessorRunner<'a, Self>) {}
}

/// Representation of the Realm config data available to Plane 0.
///
/// * ipa_width is the size of the realm protected memory space
/// * hash_algo is the hash alg used for measurements
/// * num_aux_planes indicates how many low-privilege planes exist
/// * gicv3_vtr shows part of the GICv3 configuration for the realm,
///   needed for GIC virtualisation
#[derive(Debug, Clone, Copy)]
pub struct RsiRealmConfig {
    ipa_width: u64,
    #[expect(unused)]
    hash_algo: u64,
    #[expect(unused)]
    num_aux_planes: u64,
    #[expect(unused)]
    gicv3_vtr: u64,
}

impl RsiRealmConfig {
    /// Get the IPA width of the realm
    pub fn ipa_width(&self) -> u64 {
        self.ipa_width
    }
}

impl From<mshv_realm_config> for RsiRealmConfig {
    fn from(value: mshv_realm_config) -> Self {
        RsiRealmConfig {
            ipa_width: value.ipa_width,
            hash_algo: value.algorithm,
            num_aux_planes: value.num_aux_planes,
            gicv3_vtr: value.gicv3_vtr,
        }
    }
}

impl MshvVtl {
    /// Get the realm-specific parameters from the RMM
    pub fn get_realm_config(&self) -> Result<RsiRealmConfig, Error> {
        let mut config = mshv_realm_config::default();

        // SAFETY: Calling hcl_realm_config ioctl with the correct arguments.
        unsafe {
            hcl_realm_config(self.file.as_raw_fd(), &mut config)
                .map_err(|_| Error::InvalidRegisterValue)?;
        }

        Ok(config.into())
    }

    /// Write the value of a system register for the given VTL
    pub fn rsi_sysreg_write(
        &self,
        vtl: GuestVtl,
        sysreg: u64,
        value: u64,
    ) -> Result<(), SetRegError> {
        let sysreg_write = mshv_rsi_sysreg_rw {
            vtl: vtl.into(),
            sysreg,
            value,
            ..Default::default()
        };

        // SAFETY: Calling hcl_rsi_sysreg_write ioctl with the correct arguments.
        unsafe {
            hcl_rsi_sysreg_write(self.file.as_raw_fd(), &sysreg_write)
                .map_err(SetRegError::Ioctl)?;
        }
        Ok(())
    }

    /// Read the value of a system register for the given VTL.
    pub fn rsi_sysreg_read(
        &self,
        vtl: GuestVtl,
        sysreg: u64,
        value: &mut u64,
    ) -> Result<(), GetRegError> {
        let mut sysreg_read = mshv_rsi_sysreg_rw {
            vtl: vtl.into(),
            sysreg,
            ..Default::default()
        };

        // SAFETY: Calling hcl_rsi_sysreg_read ioctl with the correct arguments.
        unsafe {
            hcl_rsi_sysreg_read(self.file.as_raw_fd(), &mut sysreg_read)
                .map_err(GetRegError::Ioctl)?;
        }

        *value = sysreg_read.value;
        Ok(())
    }

    /// Assign given memory range to the VTL.
    pub fn rsi_set_mem_perm(&self, vtl: GuestVtl, range: &MemoryRange) -> Result<(), HvError> {
        let plane = match vtl {
            GuestVtl::Vtl0 => 1,
            _ => return Err(HvError::InvalidRegisterValue),
        };

        let set_mem_perm = mshv_rsi_set_mem_perm {
            plane,
            _pad: [0; 7],
            base_addr: range.start(),
            top_addr: range.end(),
        };

        // SAFETY: Calling hcl_rsi_set_mem_perm ioctl with the correct arguments.
        unsafe {
            hcl_rsi_set_mem_perm(self.file.as_raw_fd(), &set_mem_perm)
                .map_err(|_| HvError::InvalidRegisterValue)?;
        }
        Ok(())
    }

    /// Get the ipa RIPAS state
    pub fn rsi_get_ipa_state(&self, fipa: u64) -> Result<Option<u64>, GetIpaStateError> {
        let mut plane_state = mshv_rsi_get_ipa_state {
            fipa,
            state: u64::MAX,
        };

        // SAFETY: Calling hcl_rsi_ipa_state_read ioctl with the correct arguments.
        unsafe {
            hcl_rsi_ipa_state_read(self.file.as_raw_fd(), &mut plane_state)
                .map_err(GetIpaStateError::Ioctl)?;
        }

        if plane_state.state >= u8::MAX as u64 {
            return Ok(None);
        }

        Ok(Some(plane_state.state))
    }
}

impl Hcl {
    /// Gets Realm config
    pub fn get_realm_config(&self) -> Result<RsiRealmConfig, Error> {
        self.mshv_vtl.get_realm_config()
    }

    /// sets system registers through rsi calls
    pub fn rsi_sysreg_write(
        &self,
        vtl: GuestVtl,
        sysreg: u64,
        value: u64,
    ) -> Result<(), SetRegError> {
        self.mshv_vtl.rsi_sysreg_write(vtl, sysreg, value)
    }

    /// Read a system register through RSI.
    pub fn rsi_sysreg_read(
        &self,
        vtl: GuestVtl,
        sysreg: u64,
        value: &mut u64,
    ) -> Result<(), GetRegError> {
        self.mshv_vtl.rsi_sysreg_read(vtl, sysreg, value)
    }

    /// setting memory permissions
    pub fn rsi_set_mem_perm(&self, vtl: GuestVtl, range: MemoryRange) -> Result<(), HvError> {
        self.mshv_vtl.rsi_set_mem_perm(vtl, &range)
    }

    /// getting ipa RIPAS state
    pub fn rsi_get_ipa_state(&self, fipa: u64) -> Result<Option<u64>, GetIpaStateError> {
        self.mshv_vtl.rsi_get_ipa_state(fipa)
    }
}
