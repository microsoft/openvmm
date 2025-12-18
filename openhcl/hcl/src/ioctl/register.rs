// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Routines for getting and setting register values.

use super::Backing;
use super::Hcl;
use super::HvcallRepInput;
use super::IsolationType;
use super::MshvHvcall;
use super::ProcessorRunner;
use super::hcl_get_vp_register;
use super::hcl_set_vp_register;
use super::ioctls::mshv_vp_registers;
use crate::GuestVtl;
use hvdef::HV_PARTITION_ID_SELF;
use hvdef::HV_VP_INDEX_SELF;
use hvdef::HvError;
use hvdef::HvRegisterValue;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use std::os::fd::AsRawFd;
use thiserror::Error;
use zerocopy::FromZeros;

#[cfg(guest_arch = "x86_64")]
type HvArchRegisterName = hvdef::HvX64RegisterName;

#[cfg(guest_arch = "aarch64")]
type HvArchRegisterName = hvdef::HvArm64RegisterName;

#[derive(Error, Debug)]
pub enum GetVpError {
    #[error("failed to get VP register from ioctl")]
    Ioctl(#[source] nix::Error),
    #[error("failed to get VP register from hypercall")]
    Hypercall(#[source] HvError),
    #[error("failed to get VP register from sidecar")]
    Sidecar(#[source] sidecar_client::SidecarError),
}

#[derive(Error, Debug)]
pub enum SetVpError {
    #[error("failed to set VP register via ioctl")]
    Ioctl(#[source] nix::Error),
    #[error("failed to set VP register via hypercall")]
    Hypercall(#[source] HvError),
    #[error("failed to set VP register via sidecar")]
    Sidecar(#[source] sidecar_client::SidecarError),
}

impl<'a, T: Backing<'a>> ProcessorRunner<'a, T> {
    /// Get the given register on the current VP for the given VTL.
    pub fn get_vp_register(
        &mut self,
        vtl: GuestVtl,
        name: HvArchRegisterName,
    ) -> Result<HvRegisterValue, GetVpError> {
        let mut value = [FromZeros::new_zeroed(); 1];
        self.get_vp_registers(vtl, &[name], &mut value)?;
        Ok(value[0])
    }

    /// Set the given register on the current VP for the given VTL.
    pub fn set_vp_register(
        &mut self,
        vtl: GuestVtl,
        name: HvArchRegisterName,
        value: HvRegisterValue,
    ) -> Result<(), SetVpError> {
        self.set_vp_registers(vtl, [(name, value)])
    }

    /// Get the given registers on the current VP for the given VTL.
    ///
    /// # Panics
    /// Panics if `names.len() != values.len()`.
    pub fn get_vp_registers(
        &mut self,
        vtl: GuestVtl,
        names: &[HvArchRegisterName],
        values: &mut [HvRegisterValue],
    ) -> Result<(), GetVpError> {
        assert_eq!(names.len(), values.len());
        let mut assoc = Vec::new();
        let mut offset = Vec::new();

        // Try the per-backing fast path for each register first.
        for (i, (&name, value)) in names.iter().zip(values.iter_mut()).enumerate() {
            if let Some(v) = T::try_get_reg(self, vtl, name.into()) {
                *value = v;
            } else {
                assoc.push(HvRegisterAssoc {
                    name: name.into(),
                    pad: Default::default(),
                    value: FromZeros::new_zeroed(),
                });
                offset.push(i);
            }
        }

        // Fall back to ioctl/hypercall for the remaining registers.
        self.get_regs(vtl, &mut assoc)?;
        for (&i, assoc) in offset.iter().zip(&assoc) {
            values[i] = assoc.value;
        }
        Ok(())
    }

    /// Set the given registers on the current VP for the given VTL.
    pub fn set_vp_registers<I>(&mut self, vtl: GuestVtl, values: I) -> Result<(), SetVpError>
    where
        I: IntoIterator,
        I::Item: Into<HvRegisterAssoc> + Clone,
    {
        let mut assoc = Vec::new();

        // Try the per-backing fast path for each register first.
        for HvRegisterAssoc { name, value, .. } in values.into_iter().map(Into::into) {
            if !assoc.is_empty() && T::must_flush_regs_on(self, name) {
                self.set_regs(vtl, &assoc)?;
                assoc.clear();
            }
            if !T::try_set_reg(self, vtl, name, value) {
                assoc.push(HvRegisterAssoc {
                    name,
                    pad: Default::default(),
                    value,
                });
            }
        }
        // Fall back to ioctl/hypercall for the remaining registers.
        self.set_regs(vtl, &assoc)
    }

    /// Get the given registers on the current VP for the given VTL via
    /// ioctl/hypercall, as appropriate.
    fn get_regs(&mut self, vtl: GuestVtl, regs: &mut [HvRegisterAssoc]) -> Result<(), GetVpError> {
        if regs.is_empty() {
            return Ok(());
        }

        if let Some(sidecar) = &mut self.sidecar {
            sidecar
                .get_vp_registers(vtl.into(), regs)
                .map_err(GetVpError::Sidecar)?;
        } else {
            // TODO: group up to MSHV_VP_MAX_REGISTERS regs. The kernel
            // currently has a bug where it only supports one register at a
            // time. Once that's fixed, this code could set a group of
            // registers in one ioctl.
            for reg in regs {
                if self.is_kernel_managed(reg.name.into()) {
                    let mut mshv_vp_register_args = mshv_vp_registers {
                        count: 1,
                        regs: reg,
                    };
                    // SAFETY: we know that our file is a vCPU fd, we know the kernel will only read the
                    // correct amount of memory from our pointer, and we verify the return result.
                    unsafe {
                        hcl_get_vp_register(
                            self.hcl.mshv_vtl.file.as_raw_fd(),
                            &mut mshv_vp_register_args,
                        )
                        .map_err(GetVpError::Ioctl)?;
                    }
                } else {
                    reg.value = self
                        .hcl
                        .mshv_hvcall
                        .get_vp_register_hypercall(vtl.into(), reg.name.into())?;
                }
            }
        }
        Ok(())
    }

    /// Set the given registers on the current VP for the given VTL via
    /// ioctl/hypercall, as appropriate.
    fn set_regs(&mut self, vtl: GuestVtl, regs: &[HvRegisterAssoc]) -> Result<(), SetVpError> {
        if regs.is_empty() {
            return Ok(());
        }

        if let Some(sidecar) = &mut self.sidecar {
            sidecar
                .set_vp_registers(vtl.into(), regs)
                .map_err(SetVpError::Sidecar)?;
        } else {
            // TODO: group up to MSHV_VP_MAX_REGISTERS regs. The kernel
            // currently has a bug where it only supports one register at a
            // time. Once that's fixed, this code could set a group of
            // registers in one ioctl.
            for reg in regs {
                if self.is_kernel_managed(reg.name.into()) {
                    let mshv_vp_register_args = mshv_vp_registers {
                        count: 1,
                        regs: std::ptr::from_ref(reg).cast_mut(),
                    };
                    // SAFETY: we know that our file is a vCPU fd, we know the kernel will only read the
                    // correct amount of memory from our pointer, and we verify the return result.
                    unsafe {
                        hcl_set_vp_register(
                            self.hcl.mshv_vtl.file.as_raw_fd(),
                            &mshv_vp_register_args,
                        )
                        .map_err(SetVpError::Ioctl)?;
                    }
                } else {
                    self.hcl
                        .mshv_hvcall
                        .set_vp_register_hypercall(vtl.into(), *reg)?;
                }
            }
        }
        Ok(())
    }

    /// Indicate whether the given register is managed by our kernel.
    fn is_kernel_managed(&self, name: HvArchRegisterName) -> bool {
        #[cfg(guest_arch = "x86_64")]
        if name == HvArchRegisterName::Dr6 {
            return self.hcl.dr6_shared();
        }

        is_vtl_shared_reg(name)
    }
}

impl MshvHvcall {
    /// Get the given register on the current VP for the given VTL via hypercall.
    ///
    /// Only VTL-private registers can go through this path. VTL-shared registers
    /// have to go through the kernel (either via the CPU context page or via the
    /// dedicated ioctl), as they may require special handling there.
    fn get_vp_register_hypercall(
        &self,
        vtl: Vtl,
        name: HvArchRegisterName,
    ) -> Result<HvRegisterValue, GetVpError> {
        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: HV_PARTITION_ID_SELF,
            vp_index: HV_VP_INDEX_SELF,
            target_vtl: vtl.into(),
            rsvd: [0; 3],
        };
        let mut output = [HvRegisterValue::new_zeroed()];

        // SAFETY: The input header and rep slice are the correct types for this hypercall.
        //         The hypercall output is validated right after the hypercall is issued.
        let status = unsafe {
            self.hvcall_rep(
                HypercallCode::HvCallGetVpRegisters,
                &header,
                HvcallRepInput::Elements(&[name]),
                Some(&mut output),
            )
            .expect("get_vp_registers hypercall should not fail")
        };

        // Status must be success with 1 rep completed
        status.result().map_err(GetVpError::Hypercall)?;
        assert_eq!(status.elements_processed(), 1);

        Ok(output[0])
    }

    /// Set the given register on the current VP for the given VTL via hypercall.
    ///
    /// Only VTL-private registers can go through this path. VTL-shared registers
    /// have to go through the kernel (either via the CPU context page or via the
    /// dedicated ioctl), as they may require special handling there.
    fn set_vp_register_hypercall(
        &self,
        vtl: Vtl,
        register: HvRegisterAssoc,
    ) -> Result<(), SetVpError> {
        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: HV_PARTITION_ID_SELF,
            vp_index: HV_VP_INDEX_SELF,
            target_vtl: vtl.into(),
            rsvd: [0; 3],
        };

        // SAFETY: The input header and rep slice are the correct types for this hypercall.
        //         The hypercall output is validated right after the hypercall is issued.
        let status = unsafe {
            self.hvcall_rep::<hvdef::hypercall::GetSetVpRegisters, HvRegisterAssoc, u8>(
                HypercallCode::HvCallSetVpRegisters,
                &header,
                HvcallRepInput::Elements(&[register]),
                None,
            )
            .expect("set_vp_registers hypercall should not fail")
        };

        // Status must be success
        status.result().map_err(SetVpError::Hypercall)?;
        Ok(())
    }
}

/// Indicate whether reg is shared across VTLs.
///
/// This function is not complete: DR6 may or may not be shared, depending on
/// the processor type; the caller needs to check HvRegisterVsmCapabilities.
/// Some MSRs are not included here as they are not represented in
/// HvArchRegisterName, including MSR_TSC_FREQUENCY, MSR_MCG_CAP,
/// MSR_MCG_STATUS, MSR_RESET, MSR_GUEST_IDLE, and MSR_DEBUG_DEVICE_OPTIONS.
fn is_vtl_shared_reg(reg: HvArchRegisterName) -> bool {
    #[cfg(guest_arch = "x86_64")]
    {
        matches!(
            reg,
            HvArchRegisterName::VpIndex
                | HvArchRegisterName::VpRuntime
                | HvArchRegisterName::TimeRefCount
                | HvArchRegisterName::Rax
                | HvArchRegisterName::Rbx
                | HvArchRegisterName::Rcx
                | HvArchRegisterName::Rdx
                | HvArchRegisterName::Rsi
                | HvArchRegisterName::Rdi
                | HvArchRegisterName::Rbp
                | HvArchRegisterName::Cr2
                | HvArchRegisterName::R8
                | HvArchRegisterName::R9
                | HvArchRegisterName::R10
                | HvArchRegisterName::R11
                | HvArchRegisterName::R12
                | HvArchRegisterName::R13
                | HvArchRegisterName::R14
                | HvArchRegisterName::R15
                | HvArchRegisterName::Dr0
                | HvArchRegisterName::Dr1
                | HvArchRegisterName::Dr2
                | HvArchRegisterName::Dr3
                | HvArchRegisterName::Xmm0
                | HvArchRegisterName::Xmm1
                | HvArchRegisterName::Xmm2
                | HvArchRegisterName::Xmm3
                | HvArchRegisterName::Xmm4
                | HvArchRegisterName::Xmm5
                | HvArchRegisterName::Xmm6
                | HvArchRegisterName::Xmm7
                | HvArchRegisterName::Xmm8
                | HvArchRegisterName::Xmm9
                | HvArchRegisterName::Xmm10
                | HvArchRegisterName::Xmm11
                | HvArchRegisterName::Xmm12
                | HvArchRegisterName::Xmm13
                | HvArchRegisterName::Xmm14
                | HvArchRegisterName::Xmm15
                | HvArchRegisterName::FpMmx0
                | HvArchRegisterName::FpMmx1
                | HvArchRegisterName::FpMmx2
                | HvArchRegisterName::FpMmx3
                | HvArchRegisterName::FpMmx4
                | HvArchRegisterName::FpMmx5
                | HvArchRegisterName::FpMmx6
                | HvArchRegisterName::FpMmx7
                | HvArchRegisterName::FpControlStatus
                | HvArchRegisterName::XmmControlStatus
                | HvArchRegisterName::Xfem
                | HvArchRegisterName::MsrMtrrCap
                | HvArchRegisterName::MsrMtrrDefType
                | HvArchRegisterName::MsrMtrrPhysBase0
                | HvArchRegisterName::MsrMtrrPhysBase1
                | HvArchRegisterName::MsrMtrrPhysBase2
                | HvArchRegisterName::MsrMtrrPhysBase3
                | HvArchRegisterName::MsrMtrrPhysBase4
                | HvArchRegisterName::MsrMtrrPhysBase5
                | HvArchRegisterName::MsrMtrrPhysBase6
                | HvArchRegisterName::MsrMtrrPhysBase7
                | HvArchRegisterName::MsrMtrrPhysBase8
                | HvArchRegisterName::MsrMtrrPhysBase9
                | HvArchRegisterName::MsrMtrrPhysBaseA
                | HvArchRegisterName::MsrMtrrPhysBaseB
                | HvArchRegisterName::MsrMtrrPhysBaseC
                | HvArchRegisterName::MsrMtrrPhysBaseD
                | HvArchRegisterName::MsrMtrrPhysBaseE
                | HvArchRegisterName::MsrMtrrPhysBaseF
                | HvArchRegisterName::MsrMtrrPhysMask0
                | HvArchRegisterName::MsrMtrrPhysMask1
                | HvArchRegisterName::MsrMtrrPhysMask2
                | HvArchRegisterName::MsrMtrrPhysMask3
                | HvArchRegisterName::MsrMtrrPhysMask4
                | HvArchRegisterName::MsrMtrrPhysMask5
                | HvArchRegisterName::MsrMtrrPhysMask6
                | HvArchRegisterName::MsrMtrrPhysMask7
                | HvArchRegisterName::MsrMtrrPhysMask8
                | HvArchRegisterName::MsrMtrrPhysMask9
                | HvArchRegisterName::MsrMtrrPhysMaskA
                | HvArchRegisterName::MsrMtrrPhysMaskB
                | HvArchRegisterName::MsrMtrrPhysMaskC
                | HvArchRegisterName::MsrMtrrPhysMaskD
                | HvArchRegisterName::MsrMtrrPhysMaskE
                | HvArchRegisterName::MsrMtrrPhysMaskF
                | HvArchRegisterName::MsrMtrrFix64k00000
                | HvArchRegisterName::MsrMtrrFix16k80000
                | HvArchRegisterName::MsrMtrrFix16kA0000
                | HvArchRegisterName::MsrMtrrFix4kC0000
                | HvArchRegisterName::MsrMtrrFix4kC8000
                | HvArchRegisterName::MsrMtrrFix4kD0000
                | HvArchRegisterName::MsrMtrrFix4kD8000
                | HvArchRegisterName::MsrMtrrFix4kE0000
                | HvArchRegisterName::MsrMtrrFix4kE8000
                | HvArchRegisterName::MsrMtrrFix4kF0000
                | HvArchRegisterName::MsrMtrrFix4kF8000
        )
    }

    #[cfg(guest_arch = "aarch64")]
    {
        matches!(
            reg,
            HvArchRegisterName::X0
                | HvArchRegisterName::X1
                | HvArchRegisterName::X2
                | HvArchRegisterName::X3
                | HvArchRegisterName::X4
                | HvArchRegisterName::X5
                | HvArchRegisterName::X6
                | HvArchRegisterName::X7
                | HvArchRegisterName::X8
                | HvArchRegisterName::X9
                | HvArchRegisterName::X10
                | HvArchRegisterName::X11
                | HvArchRegisterName::X12
                | HvArchRegisterName::X13
                | HvArchRegisterName::X14
                | HvArchRegisterName::X15
                | HvArchRegisterName::X16
                | HvArchRegisterName::X17
                | HvArchRegisterName::X19
                | HvArchRegisterName::X20
                | HvArchRegisterName::X21
                | HvArchRegisterName::X22
                | HvArchRegisterName::X23
                | HvArchRegisterName::X24
                | HvArchRegisterName::X25
                | HvArchRegisterName::X26
                | HvArchRegisterName::X27
                | HvArchRegisterName::X28
                | HvArchRegisterName::XFp
                | HvArchRegisterName::XLr
        )
    }
}

impl Hcl {
    /// Get the given register on the current VP for VTL 2 via hypercall.
    /// Only a select set of registers are supported; others will cause a panic.
    fn get_vp_vtl2_register(
        &self,
        name: HvArchRegisterName,
    ) -> Result<HvRegisterValue, GetVpError> {
        #[cfg(guest_arch = "x86_64")]
        let per_arch = false;

        #[cfg(guest_arch = "aarch64")]
        let per_arch = matches!(name, hvdef::HvArm64RegisterName::PrivilegesAndFeaturesInfo);

        assert!(
            matches!(
                name,
                HvArchRegisterName::GuestVsmPartitionConfig
                    | HvArchRegisterName::VsmPartitionConfig
                    | HvArchRegisterName::VsmPartitionStatus
                    | HvArchRegisterName::VsmCapabilities
                    | HvArchRegisterName::TimeRefCount
                    | HvArchRegisterName::VsmVpSecureConfigVtl0
                    | HvArchRegisterName::VsmVpSecureConfigVtl1
            ) || per_arch
        );
        self.mshv_hvcall.get_vp_register_hypercall(Vtl::Vtl2, name)
    }

    /// Set the given register on the current VP for VTL 2 via hypercall.
    /// Only a select set of registers are supported; others will cause a panic.
    fn set_vp_vtl2_register(
        &self,
        name: HvArchRegisterName,
        value: HvRegisterValue,
    ) -> Result<(), SetVpError> {
        #[cfg(guest_arch = "x86_64")]
        let per_arch = matches!(name, hvdef::HvX64RegisterName::PmTimerAssist);

        #[cfg(guest_arch = "aarch64")]
        let per_arch = false;

        assert!(
            matches!(
                name,
                HvArchRegisterName::GuestVsmPartitionConfig
                    | HvArchRegisterName::VsmPartitionConfig
            ) || per_arch
        );

        self.mshv_hvcall.set_vp_register_hypercall(
            Vtl::Vtl2,
            HvRegisterAssoc {
                name: name.into(),
                pad: Default::default(),
                value,
            },
        )
    }

    /// Gets the current hypervisor reference time.
    pub fn reference_time(&self) -> Result<u64, GetVpError> {
        Ok(self
            .get_vp_vtl2_register(HvArchRegisterName::TimeRefCount)?
            .as_u64())
    }

    /// Read the vsm capabilities register for VTL2.
    pub fn get_vsm_capabilities(&self) -> Result<hvdef::HvRegisterVsmCapabilities, GetVpError> {
        let caps = hvdef::HvRegisterVsmCapabilities::from(
            self.get_vp_vtl2_register(HvArchRegisterName::VsmCapabilities)?
                .as_u64(),
        );

        let caps = match self.isolation {
            IsolationType::None | IsolationType::Vbs => caps,
            IsolationType::Snp => hvdef::HvRegisterVsmCapabilities::new()
                .with_deny_lower_vtl_startup(caps.deny_lower_vtl_startup())
                .with_intercept_page_available(caps.intercept_page_available()),
            IsolationType::Tdx => hvdef::HvRegisterVsmCapabilities::new()
                .with_deny_lower_vtl_startup(caps.deny_lower_vtl_startup())
                .with_intercept_page_available(caps.intercept_page_available())
                .with_dr6_shared(true)
                .with_proxy_interrupt_redirect_available(caps.proxy_interrupt_redirect_available()),
        };

        assert_eq!(caps.dr6_shared(), self.dr6_shared());

        Ok(caps)
    }

    /// Get the [`hvdef::HvRegisterGuestVsmPartitionConfig`] register for VTL2.
    pub fn get_guest_vsm_partition_config(
        &self,
    ) -> Result<hvdef::HvRegisterGuestVsmPartitionConfig, GetVpError> {
        Ok(hvdef::HvRegisterGuestVsmPartitionConfig::from(
            self.get_vp_vtl2_register(HvArchRegisterName::GuestVsmPartitionConfig)?
                .as_u64(),
        ))
    }

    /// Get the [`hvdef::HvRegisterVsmPartitionStatus`] register for VTL2.
    pub fn get_vsm_partition_status(
        &self,
    ) -> Result<hvdef::HvRegisterVsmPartitionStatus, GetVpError> {
        Ok(hvdef::HvRegisterVsmPartitionStatus::from(
            self.get_vp_vtl2_register(HvArchRegisterName::VsmPartitionStatus)?
                .as_u64(),
        ))
    }

    /// Get the [`hvdef::HvPartitionPrivilege`] info. On x86_64, this uses
    /// CPUID. On aarch64, it uses get_vp_register.
    pub fn get_privileges_and_features_info(
        &self,
    ) -> Result<hvdef::HvPartitionPrivilege, GetVpError> {
        #[cfg(guest_arch = "x86_64")]
        {
            let result = safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0);
            let num = result.eax as u64 | ((result.ebx as u64) << 32);
            Ok(hvdef::HvPartitionPrivilege::from(num))
        }

        #[cfg(guest_arch = "aarch64")]
        {
            Ok(hvdef::HvPartitionPrivilege::from(
                self.get_vp_vtl2_register(HvArm64RegisterName::PrivilegesAndFeaturesInfo)?
                    .as_u64(),
            ))
        }
    }

    /// Get the [`hvdef::hypercall::HvGuestOsId`] register for the given VTL.
    // pub fn get_guest_os_id(
    //     &self,
    //     vtl: GuestVtl,
    // ) -> Result<hvdef::hypercall::HvGuestOsId, GetVpError> {
    //     Ok(hvdef::hypercall::HvGuestOsId::from(
    //         self.get_vp_register(vtl, HvArchRegisterName::GuestOsId)?
    //             .as_u64(),
    //     ))
    // }

    /// Set the [`hvdef::HvRegisterVsmPartitionConfig`] register.
    pub fn set_vtl2_vsm_partition_config(
        &self,
        vsm_config: hvdef::HvRegisterVsmPartitionConfig,
    ) -> Result<(), SetVpError> {
        self.set_vp_vtl2_register(
            HvArchRegisterName::VsmPartitionConfig,
            HvRegisterValue::from(u64::from(vsm_config)),
        )
    }

    /// Configure guest VSM.
    /// The only configuration attribute currently supported is changing the maximum number of
    /// guest-visible virtual trust levels for the partition. (VTL 1)
    pub fn set_guest_vsm_partition_config(&self, enable_guest_vsm: bool) -> Result<(), SetVpError> {
        let register_value = hvdef::HvRegisterGuestVsmPartitionConfig::new()
            .with_maximum_vtl(if enable_guest_vsm { 1 } else { 0 })
            .with_reserved(0);

        tracing::trace!(enable_guest_vsm, "set_guest_vsm_partition_config");
        if self.isolation.is_hardware_isolated() {
            unimplemented!("set_guest_vsm_partition_config");
        }

        self.set_vp_vtl2_register(
            HvArchRegisterName::GuestVsmPartitionConfig,
            HvRegisterValue::from(u64::from(register_value)),
        )
    }

    /// Sets the Power Management Timer assist in the hypervisor.
    #[cfg(guest_arch = "x86_64")]
    pub fn set_pm_timer_assist(&self, port: Option<u16>) -> Result<(), SetVpError> {
        tracing::debug!(?port, "set_pm_timer_assist");
        if self.isolation.is_hardware_isolated() {
            if port.is_some() {
                unimplemented!("set_pm_timer_assist");
            }
        }

        let val = HvRegisterValue::from(u64::from(match port {
            Some(p) => hvdef::HvPmTimerInfo::new()
                .with_port(p)
                .with_enabled(true)
                .with_width_24(false),
            None => 0.into(),
        }));

        self.set_vp_vtl2_register(HvArchRegisterName::PmTimerAssist, val)
    }

    /// Sets the Power Management Timer assist in the hypervisor.
    #[cfg(guest_arch = "aarch64")]
    pub fn set_pm_timer_assist(&self, port: Option<u16>) -> Result<(), SetVpError> {
        tracing::debug!(?port, "set_pm_timer_assist unimplemented on aarch64");
        Err(SetVpError::Hypercall(HvError::UnknownRegisterName))
    }
}
