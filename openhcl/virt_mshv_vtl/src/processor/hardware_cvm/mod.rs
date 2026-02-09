// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common processor support for hardware-isolated partitions.

pub mod apic;
pub mod tlb_lock;

use super::UhEmulationState;
use super::UhProcessor;
use crate::CvmVtl1State;
use crate::GpnSource;
use crate::GuestVsmState;
use crate::GuestVtl;
use crate::InitialVpContextOperation;
use crate::TlbFlushLockAccess;
use crate::VpStartEnableVtl;
use crate::WakeReason;
use crate::processor::HardwareIsolatedBacking;
use crate::processor::UhHypercallHandler;
use crate::validate_vtl_gpa_flags;
use cvm_tracing::CVM_ALLOWED;
use guestmem::GuestMemory;
use guestmem::GuestMemoryErrorKind;
use hv1_emulator::RequestInterrupt;
use hv1_hypercall::HvRepResult;
use hv1_structs::ProcessorSet;
use hv1_structs::VtlArray;
use hvdef::HvCacheType;
use hvdef::HvError;
use hvdef::HvInterceptAccessType;
use hvdef::HvInterruptType;
use hvdef::HvMapGpaFlags;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvRegisterVsmVpSecureVtlConfig;
use hvdef::HvResult;
use hvdef::HvVtlEntryReason;
use hvdef::HvX64InterceptMessageHeader;
use hvdef::HvX64MemoryAccessInfo;
use hvdef::HvX64MemoryInterceptMessage;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_READ;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_WRITE;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::TranslateGvaResultCode;
use std::iter::zip;
use virt::Processor;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::vp::AccessVpState;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::translate::TranslateCachingInfo;
use virt_support_x86emu::translate::TranslationRegisters;
use vm_topology::memory::AddressType;
use x86defs::cpuid;
use x86defs::cpuid::CpuidFunction;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Error type for proxy interrupt redirection failures.
#[derive(Debug, thiserror::Error)]
enum ProxyInterruptRedirectionError {
    /// Multicast is not supported for proxy interrupt redirection.
    #[error("multicast not supported")]
    MulticastNotSupported,
    /// Processor set operation failed.
    #[error("processor set operation failed")]
    ProcessorSetError,
    /// Failed to map the redirected device interrupt in VTL2 kernel.
    #[error("failed to map redirected device interrupt in VTL2 kernel: {0}")]
    MapInterruptFailed(#[source] hcl::ioctl::Error),
    /// HvCallRetargetDeviceInterrupt hypercall failed in hypervisor.
    #[error("HvCallRetargetDeviceInterrupt with proxy redirect failed in hypervisor: {0}")]
    RetargetDeviceInterruptFailed(HvError),
}

/// Manages redirected interrupt vector mapping in VTL2 for proxy interrupt redirection.
struct RedirectedVectorMapping<'a> {
    hcl: &'a hcl::ioctl::Hcl,
    apic_id: u32,
    redirected_vector: u32,
}

impl<'a> RedirectedVectorMapping<'a> {
    /// Creates a new mapping in VTL2 kernel for proxy interrupt redirection. This will be automatically
    /// unmapped when the guard is dropped unless explicitly disarmed.
    fn new(hcl: &'a hcl::ioctl::Hcl, vector: u32, apic_id: u32) -> Result<Self, hcl::ioctl::Error> {
        let redirected_vector = hcl.map_redirected_device_interrupt(vector, apic_id, true)?;
        Ok(Self {
            hcl,
            apic_id,
            redirected_vector,
        })
    }

    /// Returns the redirected vector value returned by the VTL2 kernel.
    fn redirected_vector(&self) -> u32 {
        self.redirected_vector
    }
}

impl Drop for RedirectedVectorMapping<'_> {
    /// Drop guard to unmap the interrupt vector in VTL2 kernel.
    fn drop(&mut self) {
        match self
            .hcl
            .map_redirected_device_interrupt(self.redirected_vector, self.apic_id, false)
        {
            Ok(_) => {}
            Err(err) => panic!(
                "failed to unmap VTL2 vector for proxy device interrupt: redirected_vector={}, apic_id={}, error={:?}",
                self.redirected_vector, self.apic_id, err
            ),
        }
    }
}

impl<T, B: HardwareIsolatedBacking> UhHypercallHandler<'_, '_, T, B> {
    fn validate_register_access(
        &mut self,
        target_vtl: GuestVtl,
        name: hvdef::HvRegisterName,
    ) -> HvResult<()> {
        match name.into() {
            HvX64RegisterName::Star
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask
            | HvX64RegisterName::Xfem
            | HvX64RegisterName::KernelGsBase
            | HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr2
            | HvX64RegisterName::Cr3
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Dr0
            | HvX64RegisterName::Dr1
            | HvX64RegisterName::Dr2
            | HvX64RegisterName::Dr3
            | HvX64RegisterName::Dr7
            | HvX64RegisterName::Es
            | HvX64RegisterName::Cs
            | HvX64RegisterName::Ss
            | HvX64RegisterName::Ds
            | HvX64RegisterName::Fs
            | HvX64RegisterName::Gs
            | HvX64RegisterName::Tr
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rsp
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
            | HvX64RegisterName::R15
            | HvX64RegisterName::Pat => {
                // Architectural registers can only be accessed by a higher VTL.
                if target_vtl >= self.intercepted_vtl {
                    return Err(HvError::AccessDenied);
                }
                Ok(())
            }
            HvX64RegisterName::TscAux => {
                // Architectural registers can only be accessed by a higher VTL.
                if target_vtl >= self.intercepted_vtl {
                    return Err(HvError::AccessDenied);
                }

                if self.vp.partition.caps.tsc_aux {
                    Ok(())
                } else {
                    Err(HvError::InvalidParameter)
                }
            }
            HvX64RegisterName::PendingEvent0 => {
                if target_vtl >= self.intercepted_vtl {
                    return Err(HvError::InvalidParameter);
                }
                Ok(())
            }
            HvX64RegisterName::VsmVina => {
                if target_vtl == GuestVtl::Vtl0 {
                    return Err(HvError::InvalidParameter);
                }
                Ok(())
            }
            HvX64RegisterName::CrInterceptControl
            | HvX64RegisterName::CrInterceptCr0Mask
            | HvX64RegisterName::CrInterceptCr4Mask
            | HvX64RegisterName::CrInterceptIa32MiscEnableMask => {
                if target_vtl != GuestVtl::Vtl1 {
                    return Err(HvError::AccessDenied);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn reg_access_error_to_hv_err(err: crate::processor::vp_state::Error) -> HvError {
        tracing::trace!(?err, "failed on register access");

        match err {
            super::vp_state::Error::SetRegisters(_) => HvError::OperationFailed,
            super::vp_state::Error::GetRegisters(_) => HvError::OperationFailed,
            super::vp_state::Error::InvalidValue(_, _, _) => HvError::InvalidRegisterValue,
            super::vp_state::Error::Unimplemented(_) => HvError::InvalidParameter,
            super::vp_state::Error::InvalidApicBase(_) => HvError::InvalidRegisterValue,
        }
    }

    fn get_vp_register(
        &mut self,
        vtl: GuestVtl,
        name: hvdef::HvRegisterName,
    ) -> HvResult<HvRegisterValue> {
        self.validate_register_access(vtl, name)?;
        // TODO: when get vp register i.e. in access vp state gets refactored,
        // clean this up.

        match name.into() {
            HvX64RegisterName::VsmCodePageOffsets => Ok(u64::from(
                self.vp.backing.cvm_state_mut().hv[vtl].vsm_code_page_offsets(true),
            )
            .into()),
            HvX64RegisterName::VsmCapabilities => Ok(u64::from(
                hvdef::HvRegisterVsmCapabilities::new()
                    .with_deny_lower_vtl_startup(true)
                    .with_dr6_shared(self.vp.partition.hcl.dr6_shared()),
            )
            .into()),
            HvX64RegisterName::VsmVpSecureConfigVtl0 => {
                Ok(u64::from(self.vp.get_vsm_vp_secure_config_vtl(vtl, GuestVtl::Vtl0)?).into())
            }
            HvX64RegisterName::VpAssistPage => Ok(self.vp.backing.cvm_state_mut().hv[vtl]
                .vp_assist_page()
                .into()),
            virt_msr @ (HvX64RegisterName::Star
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask
            | HvX64RegisterName::KernelGsBase) => {
                let msrs = self
                    .vp
                    .access_state(vtl.into())
                    .virtual_msrs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match virt_msr {
                    HvX64RegisterName::Star => Ok(msrs.star.into()),
                    HvX64RegisterName::Lstar => Ok(msrs.lstar.into()),
                    HvX64RegisterName::Cstar => Ok(msrs.cstar.into()),
                    HvX64RegisterName::SysenterCs => Ok(msrs.sysenter_cs.into()),
                    HvX64RegisterName::SysenterEip => Ok(msrs.sysenter_eip.into()),
                    HvX64RegisterName::SysenterEsp => Ok(msrs.sysenter_esp.into()),
                    HvX64RegisterName::Sfmask => Ok(msrs.sfmask.into()),
                    HvX64RegisterName::KernelGsBase => Ok(msrs.kernel_gs_base.into()),
                    _ => unreachable!(),
                }
            }
            HvX64RegisterName::Xfem => Ok(self
                .vp
                .access_state(vtl.into())
                .xcr()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            HvX64RegisterName::TscAux => Ok(self
                .vp
                .access_state(vtl.into())
                .tsc_aux()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            register @ (HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr2
            | HvX64RegisterName::Cr3
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Es
            | HvX64RegisterName::Cs
            | HvX64RegisterName::Ss
            | HvX64RegisterName::Ds
            | HvX64RegisterName::Fs
            | HvX64RegisterName::Gs
            | HvX64RegisterName::Tr
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rsp
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
            | HvX64RegisterName::R15) => {
                let registers = self
                    .vp
                    .access_state(vtl.into())
                    .registers()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match register {
                    HvX64RegisterName::Efer => Ok(registers.efer.into()),
                    HvX64RegisterName::Cr0 => Ok(registers.cr0.into()),
                    HvX64RegisterName::Cr2 => Ok(registers.cr2.into()),
                    HvX64RegisterName::Cr3 => Ok(registers.cr3.into()),
                    HvX64RegisterName::Cr4 => Ok(registers.cr4.into()),
                    HvX64RegisterName::Cr8 => Ok(registers.cr8.into()),
                    HvX64RegisterName::Es => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.es).into())
                    }
                    HvX64RegisterName::Cs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.cs).into())
                    }
                    HvX64RegisterName::Ss => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ss).into())
                    }
                    HvX64RegisterName::Ds => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ds).into())
                    }
                    HvX64RegisterName::Fs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.fs).into())
                    }
                    HvX64RegisterName::Gs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.gs).into())
                    }
                    HvX64RegisterName::Tr => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.tr).into())
                    }
                    HvX64RegisterName::Ldtr => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ldtr).into())
                    }
                    HvX64RegisterName::Gdtr => {
                        Ok(hvdef::HvX64TableRegister::from(registers.gdtr).into())
                    }
                    HvX64RegisterName::Idtr => {
                        Ok(hvdef::HvX64TableRegister::from(registers.idtr).into())
                    }
                    HvX64RegisterName::Rip => Ok(registers.rip.into()),
                    HvX64RegisterName::Rflags => Ok(registers.rflags.into()),
                    HvX64RegisterName::Rax => Ok(registers.rax.into()),
                    HvX64RegisterName::Rcx => Ok(registers.rcx.into()),
                    HvX64RegisterName::Rdx => Ok(registers.rdx.into()),
                    HvX64RegisterName::Rbx => Ok(registers.rbx.into()),
                    HvX64RegisterName::Rsp => Ok(registers.rsp.into()),
                    HvX64RegisterName::Rbp => Ok(registers.rbp.into()),
                    HvX64RegisterName::Rsi => Ok(registers.rsi.into()),
                    HvX64RegisterName::Rdi => Ok(registers.rdi.into()),
                    HvX64RegisterName::R8 => Ok(registers.r8.into()),
                    HvX64RegisterName::R9 => Ok(registers.r9.into()),
                    HvX64RegisterName::R10 => Ok(registers.r10.into()),
                    HvX64RegisterName::R11 => Ok(registers.r11.into()),
                    HvX64RegisterName::R12 => Ok(registers.r12.into()),
                    HvX64RegisterName::R13 => Ok(registers.r13.into()),
                    HvX64RegisterName::R14 => Ok(registers.r14.into()),
                    HvX64RegisterName::R15 => Ok(registers.r15.into()),
                    _ => unreachable!(),
                }
            }
            debug_reg @ (HvX64RegisterName::Dr0
            | HvX64RegisterName::Dr1
            | HvX64RegisterName::Dr2
            | HvX64RegisterName::Dr3
            | HvX64RegisterName::Dr7) => {
                let debug_regs = self
                    .vp
                    .access_state(vtl.into())
                    .debug_regs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match debug_reg {
                    HvX64RegisterName::Dr0 => Ok(debug_regs.dr0.into()),
                    HvX64RegisterName::Dr1 => Ok(debug_regs.dr1.into()),
                    HvX64RegisterName::Dr2 => Ok(debug_regs.dr2.into()),
                    HvX64RegisterName::Dr3 => Ok(debug_regs.dr3.into()),
                    HvX64RegisterName::Dr7 => Ok(debug_regs.dr7.into()),
                    _ => unreachable!(),
                }
            }
            HvX64RegisterName::Pat => Ok(self
                .vp
                .access_state(vtl.into())
                .pat()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            synic_reg @ (HvX64RegisterName::Sint0
            | HvX64RegisterName::Sint1
            | HvX64RegisterName::Sint2
            | HvX64RegisterName::Sint3
            | HvX64RegisterName::Sint4
            | HvX64RegisterName::Sint5
            | HvX64RegisterName::Sint6
            | HvX64RegisterName::Sint7
            | HvX64RegisterName::Sint8
            | HvX64RegisterName::Sint9
            | HvX64RegisterName::Sint10
            | HvX64RegisterName::Sint11
            | HvX64RegisterName::Sint12
            | HvX64RegisterName::Sint13
            | HvX64RegisterName::Sint14
            | HvX64RegisterName::Sint15
            | HvX64RegisterName::Scontrol
            | HvX64RegisterName::Sversion
            | HvX64RegisterName::Sifp
            | HvX64RegisterName::Sipp
            | HvX64RegisterName::Eom
            | HvX64RegisterName::Stimer0Config
            | HvX64RegisterName::Stimer0Count
            | HvX64RegisterName::Stimer1Config
            | HvX64RegisterName::Stimer1Count
            | HvX64RegisterName::Stimer2Config
            | HvX64RegisterName::Stimer2Count
            | HvX64RegisterName::Stimer3Config
            | HvX64RegisterName::Stimer3Count
            | HvX64RegisterName::VsmVina) => self.vp.backing.cvm_state_mut().hv[vtl]
                .synic
                .read_reg(synic_reg.into()),
            HvX64RegisterName::ApicBase => Ok(self.vp.backing.cvm_state_mut().lapics[vtl]
                .lapic
                .apic_base()
                .into()),
            control_reg @ (HvX64RegisterName::CrInterceptControl
            | HvX64RegisterName::CrInterceptCr0Mask
            | HvX64RegisterName::CrInterceptCr4Mask
            | HvX64RegisterName::CrInterceptIa32MiscEnableMask) => {
                let vtl1 = self
                    .vp
                    .backing
                    .cvm_state_mut()
                    .vtl1
                    .as_ref()
                    .ok_or(HvError::InvalidVtlState)?;
                Ok(match control_reg {
                    HvX64RegisterName::CrInterceptControl => {
                        u64::from(vtl1.reg_intercept.intercept_control)
                    }
                    HvX64RegisterName::CrInterceptCr0Mask => vtl1.reg_intercept.cr0_mask,
                    HvX64RegisterName::CrInterceptCr4Mask => vtl1.reg_intercept.cr4_mask,
                    HvX64RegisterName::CrInterceptIa32MiscEnableMask => {
                        vtl1.reg_intercept.ia32_misc_enable_mask
                    }
                    _ => unreachable!(),
                }
                .into())
            }
            _ => {
                tracing::error!(
                    CVM_ALLOWED,
                    ?name,
                    "guest invoked getvpregister with unsupported register"
                );
                Err(HvError::InvalidParameter)
            }
        }
    }

    fn set_vp_register(
        &mut self,
        vtl: GuestVtl,
        reg: &hvdef::hypercall::HvRegisterAssoc,
    ) -> HvResult<()> {
        self.validate_register_access(vtl, reg.name)?;
        // TODO CVM:
        // - when access vp state has support for single registers, clean this
        //   up.
        // - validate the values being set, e.g. that addresses are canonical,
        //   that efer and pat make sense, etc. Similar validation is needed in
        //   the write_msr path.

        match HvX64RegisterName::from(reg.name) {
            HvX64RegisterName::VsmPartitionConfig => self.vp.set_vsm_partition_config(
                HvRegisterVsmPartitionConfig::from(reg.value.as_u64()),
                vtl,
            ),
            HvX64RegisterName::VsmVpSecureConfigVtl0 => self.vp.set_vsm_vp_secure_config_vtl(
                vtl,
                GuestVtl::Vtl0,
                HvRegisterVsmVpSecureVtlConfig::from(reg.value.as_u64()),
            ),
            HvX64RegisterName::VpAssistPage => {
                let self_index = self.vp.vp_index();
                self.vp.backing.cvm_state_mut().hv[vtl]
                    .msr_write_vp_assist_page(
                        reg.value.as_u64(),
                        &mut CvmVtlProtectAccess {
                            vtl,
                            protector: B::cvm_partition_state(self.vp.shared)
                                .isolated_memory_protector
                                .as_ref(),
                            tlb_access: &mut B::tlb_flush_lock_access(
                                Some(self_index),
                                self.vp.partition,
                                self.vp.shared,
                            ),
                            guest_memory: &self.vp.partition.gm[vtl],
                        },
                    )
                    .map_err(|_| HvError::InvalidRegisterValue)
            }
            virt_msr @ (HvX64RegisterName::Star
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask) => {
                // Checked that the intercepted vtl is strictly higher than the
                // target VTL, so no need to check for registered intercepts.

                let mut msrs = self
                    .vp
                    .access_state(vtl.into())
                    .virtual_msrs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match virt_msr {
                    HvX64RegisterName::Star => msrs.star = reg.value.as_u64(),
                    HvX64RegisterName::Cstar => msrs.cstar = reg.value.as_u64(),
                    HvX64RegisterName::Lstar => msrs.lstar = reg.value.as_u64(),
                    HvX64RegisterName::SysenterCs => msrs.sysenter_cs = reg.value.as_u64(),
                    HvX64RegisterName::SysenterEip => msrs.sysenter_eip = reg.value.as_u64(),
                    HvX64RegisterName::SysenterEsp => msrs.sysenter_esp = reg.value.as_u64(),
                    HvX64RegisterName::Sfmask => msrs.sfmask = reg.value.as_u64(),
                    _ => unreachable!(),
                }
                self.vp
                    .access_state(vtl.into())
                    .set_virtual_msrs(&msrs)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            HvX64RegisterName::TscAux => {
                // Checked that the intercepted vtl is strictly higher than the
                // target VTL, so no need to check for registered intercepts.
                self.vp
                    .access_state(vtl.into())
                    .set_tsc_aux(&virt::vp::TscAux {
                        value: reg.value.as_u64(),
                    })
                    .map_err(Self::reg_access_error_to_hv_err)
            }

            debug_reg @ (HvX64RegisterName::Dr3 | HvX64RegisterName::Dr7) => {
                let mut debug_registers = self
                    .vp
                    .access_state(vtl.into())
                    .debug_regs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match debug_reg {
                    HvX64RegisterName::Dr3 => debug_registers.dr3 = reg.value.as_u64(),
                    HvX64RegisterName::Dr7 => debug_registers.dr7 = reg.value.as_u64(),
                    _ => unreachable!(),
                }

                self.vp
                    .access_state(vtl.into())
                    .set_debug_regs(&debug_registers)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            HvX64RegisterName::Pat => self
                .vp
                .access_state(vtl.into())
                .set_pat(&virt::vp::Pat {
                    value: reg.value.as_u64(),
                })
                .map_err(Self::reg_access_error_to_hv_err),
            register @ (HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rsp) => {
                // Checked that the intercepted vtl is strictly higher than the
                // target VTL, so no need to check for registered intercepts.
                let mut registers = self
                    .vp
                    .access_state(vtl.into())
                    .registers()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match register {
                    HvX64RegisterName::Efer => registers.efer = reg.value.as_u64(),
                    HvX64RegisterName::Cr0 => registers.cr0 = reg.value.as_u64(),
                    HvX64RegisterName::Cr4 => registers.cr4 = reg.value.as_u64(),
                    HvX64RegisterName::Cr8 => registers.cr8 = reg.value.as_u64(),
                    HvX64RegisterName::Ldtr => {
                        registers.ldtr = hvdef::HvX64SegmentRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Gdtr => {
                        registers.gdtr = hvdef::HvX64TableRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Idtr => {
                        registers.idtr = hvdef::HvX64TableRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Rip => registers.rip = reg.value.as_u64(),
                    HvX64RegisterName::Rflags => registers.rflags = reg.value.as_u64(),
                    HvX64RegisterName::Rsp => registers.rsp = reg.value.as_u64(),
                    _ => unreachable!(),
                }
                self.vp
                    .access_state(vtl.into())
                    .set_registers(&registers)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            synic_reg @ (HvX64RegisterName::Sint0
            | HvX64RegisterName::Sint1
            | HvX64RegisterName::Sint2
            | HvX64RegisterName::Sint3
            | HvX64RegisterName::Sint4
            | HvX64RegisterName::Sint5
            | HvX64RegisterName::Sint6
            | HvX64RegisterName::Sint7
            | HvX64RegisterName::Sint8
            | HvX64RegisterName::Sint9
            | HvX64RegisterName::Sint10
            | HvX64RegisterName::Sint11
            | HvX64RegisterName::Sint12
            | HvX64RegisterName::Sint13
            | HvX64RegisterName::Sint14
            | HvX64RegisterName::Sint15
            | HvX64RegisterName::Scontrol
            | HvX64RegisterName::Sversion
            | HvX64RegisterName::Sifp
            | HvX64RegisterName::Sipp
            | HvX64RegisterName::Eom
            | HvX64RegisterName::Stimer0Config
            | HvX64RegisterName::Stimer0Count
            | HvX64RegisterName::Stimer1Config
            | HvX64RegisterName::Stimer1Count
            | HvX64RegisterName::Stimer2Config
            | HvX64RegisterName::Stimer2Count
            | HvX64RegisterName::Stimer3Config
            | HvX64RegisterName::Stimer3Count
            | HvX64RegisterName::VsmVina) => {
                let self_index = self.vp.vp_index();
                self.vp.backing.cvm_state_mut().hv[vtl].synic.write_reg(
                    synic_reg.into(),
                    reg.value,
                    &mut CvmVtlProtectAccess {
                        vtl,
                        protector: B::cvm_partition_state(self.vp.shared)
                            .isolated_memory_protector
                            .as_ref(),
                        tlb_access: &mut B::tlb_flush_lock_access(
                            Some(self_index),
                            self.vp.partition,
                            self.vp.shared,
                        ),
                        guest_memory: &self.vp.partition.gm[vtl],
                    },
                )
            }
            HvX64RegisterName::ApicBase => {
                // No changes are allowed on this path.
                let current = self.vp.backing.cvm_state_mut().lapics[vtl]
                    .lapic
                    .apic_base();
                if reg.value.as_u64() != current {
                    return Err(HvError::InvalidParameter);
                }
                Ok(())
            }
            HvX64RegisterName::PendingEvent0 => {
                // Currently no support to inject the event into a VTL other
                // than VTL 0.
                if vtl != GuestVtl::Vtl0 {
                    return Err(HvError::AccessDenied);
                }

                self.set_vtl0_pending_event(HvX64PendingExceptionEvent::from(reg.value.as_u128()))
            }
            HvX64RegisterName::CrInterceptControl => {
                if vtl != GuestVtl::Vtl1 {
                    return Err(HvError::AccessDenied);
                }

                self.set_vtl1_cr_intercept_control(hvdef::HvRegisterCrInterceptControl::from(
                    reg.value.as_u64(),
                ))
            }
            mask_reg @ (HvX64RegisterName::CrInterceptCr0Mask
            | HvX64RegisterName::CrInterceptCr4Mask
            | HvX64RegisterName::CrInterceptIa32MiscEnableMask) => {
                let vtl1 = self
                    .vp
                    .backing
                    .cvm_state_mut()
                    .vtl1
                    .as_mut()
                    .ok_or(HvError::InvalidVtlState)?;
                match mask_reg {
                    HvX64RegisterName::CrInterceptCr0Mask => {
                        vtl1.reg_intercept.cr0_mask = reg.value.as_u64();
                    }
                    HvX64RegisterName::CrInterceptCr4Mask => {
                        vtl1.reg_intercept.cr4_mask = reg.value.as_u64();
                    }
                    HvX64RegisterName::CrInterceptIa32MiscEnableMask => {
                        vtl1.reg_intercept.ia32_misc_enable_mask = reg.value.as_u64();
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }
            _ => {
                tracing::error!(
                    CVM_ALLOWED,
                    reg = ?reg.name,
                    "guest invoked SetVpRegisters with unsupported register",
                );
                Err(HvError::InvalidParameter)
            }
        }
    }

    fn set_vtl0_pending_event(&mut self, event: HvX64PendingExceptionEvent) -> HvResult<()> {
        let set_event = if event.event_pending() {
            // Only exception events are supported.
            if event.event_type() != hvdef::HV_X64_PENDING_EVENT_EXCEPTION {
                return Err(HvError::InvalidRegisterValue);
            }

            // Only recognized architectural exceptions are permitted.  This
            // excludes exceptions not common to Intel/AMD platforms
            // (#VE, #VC, #SX, #HV are excluded).

            if (event.vector() > (x86defs::Exception::CONTROL_PROTECTION_EXCEPTION.0 as u16))
                || !matches!(
                    x86defs::Exception(event.vector() as u8),
                    x86defs::Exception::DIVIDE_ERROR
                        | x86defs::Exception::DEBUG
                        | x86defs::Exception::BREAKPOINT
                        | x86defs::Exception::OVERFLOW
                        | x86defs::Exception::BOUND_RANGE_EXCEEDED
                        | x86defs::Exception::INVALID_OPCODE
                        | x86defs::Exception::DEVICE_NOT_AVAILABLE
                        | x86defs::Exception::DOUBLE_FAULT
                        | x86defs::Exception::INVALID_TSS
                        | x86defs::Exception::SEGMENT_NOT_PRESENT
                        | x86defs::Exception::STACK_SEGMENT_FAULT
                        | x86defs::Exception::GENERAL_PROTECTION_FAULT
                        | x86defs::Exception::PAGE_FAULT
                        | x86defs::Exception::FLOATING_POINT_EXCEPTION
                        | x86defs::Exception::ALIGNMENT_CHECK
                        | x86defs::Exception::MACHINE_CHECK
                        | x86defs::Exception::SIMD_FLOATING_POINT_EXCEPTION
                        | x86defs::Exception::CONTROL_PROTECTION_EXCEPTION
                )
            {
                return Err(HvError::InvalidRegisterValue);
            }

            // Error codes are only permitted for recognized architecural
            // exceptions.

            if event.deliver_error_code()
                && !matches!(
                    x86defs::Exception(event.vector().try_into().unwrap()),
                    x86defs::Exception::DOUBLE_FAULT
                        | x86defs::Exception::INVALID_TSS
                        | x86defs::Exception::SEGMENT_NOT_PRESENT
                        | x86defs::Exception::STACK_SEGMENT_FAULT
                        | x86defs::Exception::GENERAL_PROTECTION_FAULT
                        | x86defs::Exception::PAGE_FAULT
                        | x86defs::Exception::ALIGNMENT_CHECK
                        | x86defs::Exception::CONTROL_PROTECTION_EXCEPTION
                )
            {
                return Err(HvError::InvalidRegisterValue);
            }

            Some(event)
        } else {
            None
        };

        self.vp
            .backing
            .cvm_state_mut()
            .vtl1
            .as_mut()
            .unwrap()
            .vtl0_exit_pending_event = set_event;

        self.vp.exit_activities[GuestVtl::Vtl0].set_pending_event(true);

        Ok(())
    }

    fn set_vtl1_cr_intercept_control(
        &mut self,
        intercept_control: hvdef::HvRegisterCrInterceptControl,
    ) -> HvResult<()> {
        // We support intercepting all writes except msr_sgx_launch_control_write, but no reads.
        let supported_controls = hvdef::HvRegisterCrInterceptControl::new()
            .with_cr0_write(true)
            .with_cr4_write(true)
            .with_xcr0_write(true)
            .with_ia32_misc_enable_write(true)
            .with_msr_lstar_write(true)
            .with_msr_star_write(true)
            .with_msr_cstar_write(true)
            .with_apic_base_msr_write(true)
            .with_msr_efer_write(true)
            .with_gdtr_write(true)
            .with_idtr_write(true)
            .with_ldtr_write(true)
            .with_tr_write(true)
            .with_msr_sysenter_cs_write(true)
            .with_msr_sysenter_eip_write(true)
            .with_msr_sysenter_esp_write(true)
            .with_msr_sfmask_write(true)
            .with_msr_tsc_aux_write(true)
            .with_msr_xss_write(true)
            .with_msr_scet_write(true)
            .with_msr_pls_ssp_write(true)
            .with_msr_interrupt_ssp_table_addr_write(true);

        if u64::from(intercept_control) & !u64::from(supported_controls) != 0 {
            return Err(HvError::InvalidRegisterValue);
        }

        B::cr_intercept_registration(self.vp, intercept_control);

        self.vp
            .backing
            .cvm_state_mut()
            .vtl1
            .as_mut()
            .unwrap()
            .reg_intercept
            .intercept_control = intercept_control;
        Ok(())
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::ModifySparseGpaPageHostVisibility
    for UhHypercallHandler<'_, '_, T, B>
{
    fn modify_gpa_visibility(
        &mut self,
        partition_id: u64,
        visibility: HostVisibilityType,
        gpa_pages: &[u64],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        tracing::debug!(
            ?visibility,
            pages = gpa_pages.len(),
            "modify_gpa_visibility"
        );

        if self.vp.cvm_partition().hide_isolation {
            return Err((HvError::AccessDenied, 0));
        }

        let shared = match visibility {
            HostVisibilityType::PRIVATE => false,
            HostVisibilityType::SHARED => true,
            _ => return Err((HvError::InvalidParameter, 0)),
        };

        self.vp
            .cvm_partition()
            .isolated_memory_protector
            .change_host_visibility(
                self.intercepted_vtl,
                shared,
                gpa_pages,
                &mut self.vp.tlb_flush_lock_access(),
            )
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> UhHypercallHandler<'_, '_, T, B> {
    fn retarget_physical_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        vector: u32,
        multicast: bool,
        target_processors: ProcessorSet<'_>,
    ) -> HvResult<()> {
        let entry = hvdef::hypercall::InterruptEntry {
            source: hvdef::hypercall::HvInterruptSource::MSI,
            rsvd: 0,
            data: [address as u32, data],
        };

        // Before dispatching retarget_device_interrupt, add the device vector
        // to partition global device vector table and issue `proxy_irr_blocked`
        // filter wake request to other VPs
        self.vp.partition.request_proxy_irr_filter_update(
            self.intercepted_vtl,
            vector as u8,
            self.vp.vp_index(),
        );

        // Update `proxy_irr_blocked` for this VP itself
        self.vp.update_proxy_irr_filter(self.intercepted_vtl);

        if self.vp.cvm_partition().proxy_interrupt_redirect {
            // Try proxy interrupt redirection. Fall back to normal proxy delivery upon any error.
            match self.try_proxy_interrupt_redirection(
                device_id,
                entry,
                vector,
                multicast,
                &target_processors,
            ) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        error = %err,
                        "proxy interrupt redirection failed, using normal proxy delivery"
                    );
                }
            }
        }

        self.vp.partition.hcl.retarget_device_interrupt(
            device_id,
            entry,
            vector,
            multicast,
            target_processors,
            false,
        )
    }

    /// Request redirection of interrupts from VTL0 owned devices to VTL2 via posted interrupt mechanism.
    /// This is useful performance optimization when VTL0 doesn't have posted interrupt support.
    fn try_proxy_interrupt_redirection(
        &mut self,
        device_id: u64,
        entry: hvdef::hypercall::InterruptEntry,
        vector: u32,
        multicast: bool,
        target_processors: &ProcessorSet<'_>,
    ) -> Result<(), ProxyInterruptRedirectionError> {
        // Proxy interrupt redirection doesn't support multicast.
        if multicast {
            return Err(ProxyInterruptRedirectionError::MulticastNotSupported);
        }

        // Register the interrupt handler in VTL2 for only the first processor in the target set.
        // This is safe because we expose only this single processor in the target processor set
        // when forwarding the hypercall to the hypervisor.

        // Get the first processor from the target processor set.
        let first_processor_index = target_processors
            .iter()
            .next()
            .ok_or(ProxyInterruptRedirectionError::ProcessorSetError)?;
        let first_apic_id = self
            .vp
            .partition
            .vps
            .get(first_processor_index as usize)
            .ok_or(ProxyInterruptRedirectionError::ProcessorSetError)?
            .vp_info
            .apic_id;

        // Map the interrupt vector in VTL2 and create guard for automatic cleanup.
        let guard = RedirectedVectorMapping::new(&self.vp.partition.hcl, vector, first_apic_id)
            .map_err(ProxyInterruptRedirectionError::MapInterruptFailed)?;

        // Create new sparse ProcessorSet containing only the first processor.
        let mask_index = first_processor_index / 64;
        let processor_mask = 1u64 << (first_processor_index % 64);
        let masks = [processor_mask];
        let redirected_processor = ProcessorSet::from_processor_masks(1u64 << mask_index, &masks)
            .ok_or(ProxyInterruptRedirectionError::ProcessorSetError)?;
        let redirected_vector = guard.redirected_vector();

        // Issue HvCallRetargetDeviceInterrupt hypercall with posted interrupt redirection enabled
        self.vp
            .partition
            .hcl
            .retarget_device_interrupt(
                device_id,
                entry,
                redirected_vector,
                multicast,
                redirected_processor,
                true,
            )
            .map_err(ProxyInterruptRedirectionError::RetargetDeviceInterruptFailed)?;

        // Disarm the guard upon success to prevent unmapping.
        std::mem::forget(guard);

        // Record the redirection for debugging/inspection purposes.
        let mut proxy_redirect_interrupts = self
            .vp
            .cvm_partition()
            .vp_inner(first_processor_index)
            .proxy_redirect_interrupts
            .lock();
        proxy_redirect_interrupts.insert(
            redirected_vector,
            crate::ProxyRedirectVectorInfo {
                device_id,
                original_vector: vector,
            },
        );
        tracelimit::info_ratelimited!(
            CVM_ALLOWED,
            device_id,
            target_vp_index = first_processor_index,
            original_vector = vector,
            redirected_vector,
            "proxy interrupt redirection successfully mapped"
        );
        Ok(())
    }

    pub(crate) fn hcvm_validate_flush_inputs(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        allow_extended_ranges: bool,
    ) -> HvResult<()> {
        let valid_flags = HvFlushFlags::new()
            .with_all_processors(true)
            .with_all_virtual_address_spaces(true)
            .with_non_global_mappings_only(true)
            .with_use_extended_range_format(allow_extended_ranges);

        if u64::from(flags) & !u64::from(valid_flags) != 0 {
            return Err(HvError::InvalidParameter);
        }
        if processor_set.is_empty() && !flags.all_processors() {
            return Err(HvError::InvalidParameter);
        }
        // TODO should we check the all_virtual_address_spaces flag? we don't check this flag or the address space input arg anywhere in the hcl
        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::GetVpRegisters
    for UhHypercallHandler<'_, '_, T, B>
{
    fn get_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::HvRegisterName],
        output: &mut [HvRegisterValue],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::AccessDenied, 0));
        }

        let vtl = self
            .target_vtl_no_higher(vtl.unwrap_or_else(|| self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;

        for (i, (&name, output)) in zip(registers, output).enumerate() {
            *output = self.get_vp_register(vtl, name).map_err(|e| (e, i))?;
        }

        Ok(())
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::RetargetDeviceInterrupt
    for UhHypercallHandler<'_, '_, T, B>
{
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: hv1_hypercall::HvInterruptParameters<'_>,
    ) -> HvResult<()> {
        let hv1_hypercall::HvInterruptParameters {
            vector,
            multicast,
            target_processors,
        } = params;
        // It is unknown whether the interrupt is physical or virtual, so try both. Note that the
        // actual response from the hypervisor can't really be trusted so:
        // 1. Always invoke the virtual interrupt retargeting.
        // 2. A failure from the physical interrupt retargeting is not necessarily a sign of a
        // malicious hypervisor or a buggy guest, since the target could simply be a virtual one.
        let hv_result = self.retarget_physical_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        let virtual_result = self.retarget_virtual_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        hv_result.or(virtual_result)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::SetVpRegisters
    for UhHypercallHandler<'_, '_, T, B>
{
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::hypercall::HvRegisterAssoc],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::InvalidVpIndex, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(vtl.unwrap_or_else(|| self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;

        for (i, reg) in registers.iter().enumerate() {
            self.set_vp_register(target_vtl, reg).map_err(|e| (e, i))?;
        }

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::VtlCall for UhHypercallHandler<'_, '_, T, B> {
    fn is_vtl_call_allowed(&self) -> bool {
        // Only allowed from VTL 0
        if self.intercepted_vtl != GuestVtl::Vtl0 {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                "vtl call not allowed from vtl {:?}",
                self.intercepted_vtl
            );
            false
        } else if self.vp.backing.cvm_state().vtl1.is_none() {
            // VTL 1 must be active on the vp
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                "vtl call not allowed because vtl 1 is not enabled"
            );
            false
        } else {
            true
        }
    }

    fn vtl_call(&mut self) {
        self.vp.raise_vtl(
            self.intercepted_vtl,
            GuestVtl::Vtl1,
            HvVtlEntryReason::VTL_CALL,
        );
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::VtlReturn for UhHypercallHandler<'_, '_, T, B> {
    fn is_vtl_return_allowed(&self) -> bool {
        if self.intercepted_vtl != GuestVtl::Vtl1 {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                "vtl return not allowed from vtl {:?}",
                self.intercepted_vtl
            );
        }

        // Only allowed from VTL 1
        self.intercepted_vtl != GuestVtl::Vtl0
    }

    fn vtl_return(&mut self, fast: bool) {
        self.vp.unlock_tlb_lock(Vtl::Vtl1);

        let hv = &mut self.vp.backing.cvm_state_mut().hv[GuestVtl::Vtl1];
        if hv.synic.vina().auto_reset() {
            hv.set_vina_asserted(false);
        }

        B::switch_vtl(self.vp, self.intercepted_vtl, GuestVtl::Vtl0);

        // TODO CVM GUEST_VSM:
        // - rewind interrupts

        if !fast {
            let [rax, rcx] = self.vp.backing.cvm_state_mut().hv[GuestVtl::Vtl1].return_registers();
            let mut vp_state = self.vp.access_state(Vtl::Vtl0);
            let mut registers = vp_state.registers().unwrap();
            registers.rax = rax;
            registers.rcx = rcx;

            vp_state.set_registers(&registers).unwrap();
        }
    }
}

impl<T, B: HardwareIsolatedBacking>
    hv1_hypercall::StartVirtualProcessor<hvdef::hypercall::InitialVpContextX64>
    for UhHypercallHandler<'_, '_, T, B>
{
    fn start_virtual_processor(
        &mut self,
        partition_id: u64,
        target_vp: u32,
        target_vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> HvResult<()> {
        tracing::debug!(
            vp_index = self.vp.vp_index().index(),
            target_vp,
            ?target_vtl,
            "HvStartVirtualProcessor"
        );

        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        if target_vp == self.vp.vp_index().index()
            || target_vp as usize >= self.vp.partition.vps.len()
        {
            return Err(HvError::InvalidVpIndex);
        }

        let target_vtl = self.target_vtl_no_higher(target_vtl)?;
        let target_vp_inner = self.vp.cvm_partition().vp_inner(target_vp);

        // The target VTL must have been enabled.
        if target_vtl == GuestVtl::Vtl1 && !*target_vp_inner.vtl1_enable_called.lock() {
            return Err(HvError::InvalidVpState);
        }

        // If lower VTL startup has been suppressed, then the request must be
        // coming from a secure VTL. We know guest VSM has been enabled from the
        // previous check.
        if self.intercepted_vtl == GuestVtl::Vtl0
            && self.vp.cvm_partition().is_lower_vtl_startup_denied()
        {
            return Err(HvError::AccessDenied);
        }

        // The StartVp hypercall is intended to work like an INIT, so it
        // theoretically can be called on an already running VP. However, this
        // makes it more difficult to reason about how to interact with higher
        // vtls and with the DenyLowerVtlStartup, and in practice, it's not clear
        // whether any guest OS does this. For now, if guest vsm is enabled,
        // simplify by disallowing repeated vp startup. Revisit this later if it
        // becomes a problem. Note that this will not apply to non-hardware cvms
        // as this may regress existing VMs.

        // After this check, there can be no more failures, so try setting the
        // fact that the VM started to true here.
        if target_vp_inner
            .started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(HvError::InvalidVpState);
        }

        let start_state = VpStartEnableVtl {
            operation: InitialVpContextOperation::StartVp,
            context: *vp_context,
        };

        *self
            .vp
            .cvm_partition()
            .vp_inner(target_vp)
            .hv_start_enable_vtl_vp[target_vtl]
            .lock() = Some(Box::new(start_state));
        self.vp.partition.vps[target_vp as usize]
            .wake(target_vtl, WakeReason::HV_START_ENABLE_VP_VTL);

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::ModifyVtlProtectionMask
    for UhHypercallHandler<'_, '_, T, B>
{
    fn modify_vtl_protection_mask(
        &mut self,
        partition_id: u64,
        map_flags: HvMapGpaFlags,
        target_vtl: Option<Vtl>,
        gpa_pages: &[u64],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(target_vtl.unwrap_or(self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;
        if target_vtl == GuestVtl::Vtl0 {
            return Err((HvError::InvalidParameter, 0));
        }

        let protector = &self.vp.cvm_partition().isolated_memory_protector;

        // A VTL cannot change its own VTL permissions until it has enabled VTL protection and
        // configured default permissions. Higher VTLs are not under this restriction (as they may
        // need to apply default permissions before VTL protection is enabled).
        if target_vtl == self.intercepted_vtl && !protector.vtl1_protections_enabled() {
            return Err((HvError::AccessDenied, 0));
        }

        // VTL 1 must be enabled already.
        let guest_vsm_lock = self.vp.cvm_partition().guest_vsm.read();
        let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
            return Err((HvError::InvalidVtlState, 0));
        };

        if !validate_vtl_gpa_flags(
            map_flags,
            vtl1.mbec_enabled,
            vtl1.shadow_supervisor_stack_enabled,
        ) {
            return Err((HvError::InvalidRegisterValue, 0));
        }

        // The contract for VSM is that the VTL protections describe what the
        // lower VTLs are allowed to access. Hardware CVMs set the protections
        // on the VTL itself. Therefore, for a hardware CVM, given that only VTL
        // 1 can set the protections, the permissions should be changed for VTL
        // 0.
        protector.change_vtl_protections(
            GuestVtl::Vtl0,
            gpa_pages,
            map_flags,
            &mut self.vp.tlb_flush_lock_access(),
        )
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::QuerySparseGpaPageHostVisibility
    for UhHypercallHandler<'_, '_, T, B>
{
    fn query_gpa_visibility(
        &mut self,
        partition_id: u64,
        gpa_pages: &[u64],
        host_visibility: &mut [HostVisibilityType],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if self.vp.cvm_partition().hide_isolation {
            return Err((HvError::AccessDenied, 0));
        }

        self.vp
            .cvm_partition()
            .isolated_memory_protector
            .query_host_visibility(gpa_pages, host_visibility)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::EnablePartitionVtl
    for UhHypercallHandler<'_, '_, T, B>
{
    fn enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        flags: hvdef::hypercall::EnablePartitionVtlFlags,
    ) -> HvResult<()> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        let target_vtl = GuestVtl::try_from(target_vtl).map_err(|_| HvError::AccessDenied)?;
        if target_vtl != GuestVtl::Vtl1 {
            return Err(HvError::AccessDenied);
        }

        if flags.enable_supervisor_shadow_stack() || flags.enable_hardware_hvpt() {
            return Err(HvError::InvalidParameter);
        }

        let mut gvsm_state = self.vp.cvm_partition().guest_vsm.write();

        match *gvsm_state {
            GuestVsmState::NotPlatformSupported => return Err(HvError::AccessDenied),
            GuestVsmState::NotGuestEnabled => (),
            GuestVsmState::Enabled { vtl1: _ } => {
                // VTL 1 cannot be already enabled
                return Err(HvError::VtlAlreadyEnabled);
            }
        }

        self.vp.partition.hcl.enable_partition_vtl(
            target_vtl,
            // These flags are managed and enforced internally; CVMs can't rely
            // on the hypervisor
            0.into(),
        )?;

        *gvsm_state = GuestVsmState::Enabled {
            vtl1: CvmVtl1State::new(flags.enable_mbec()),
        };

        let protector = &self.vp.cvm_partition().isolated_memory_protector;

        // Grant VTL 1 access to lower VTL memory
        tracing::debug!("Granting VTL 1 access to lower VTL memory");
        protector.change_default_vtl_protections(
            GuestVtl::Vtl1,
            hvdef::HV_MAP_GPA_PERMISSIONS_ALL,
            &mut self.vp.tlb_flush_lock_access(),
        )?;

        tracing::debug!("Successfully granted vtl 1 access to lower vtl memory");

        tracing::info!(CVM_ALLOWED, "Enabled vtl 1 on the partition");

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking>
    hv1_hypercall::EnableVpVtl<hvdef::hypercall::InitialVpContextX64>
    for UhHypercallHandler<'_, '_, T, B>
{
    fn enable_vp_vtl(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> HvResult<()> {
        tracing::debug!(
            vp_index = self.vp.vp_index().index(),
            target_vp = vp_index,
            ?vtl,
            "HvEnableVpVtl"
        );
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        if vp_index as usize >= self.vp.partition.vps.len() {
            return Err(HvError::InvalidVpIndex);
        }

        let vtl = GuestVtl::try_from(vtl).map_err(|_| HvError::InvalidParameter)?;
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        // If handling on behalf of VTL 0, then lock to make sure that no other
        // VP makes this call on behalf of VTL 0.
        let gvsm_state = {
            let guest_vsm_lock = self.vp.cvm_partition().guest_vsm.write();

            // Should be enabled on the partition
            let vtl1 = parking_lot::RwLockWriteGuard::try_map(guest_vsm_lock, |gvsm| {
                if let GuestVsmState::Enabled { vtl1, .. } = &mut *gvsm {
                    Some(vtl1)
                } else {
                    None
                }
            })
            .map_err(|_| HvError::InvalidVtlState)?;

            let current_vp_index = self.vp.vp_index().index();

            // A higher VTL can only be enabled on the current processor to make
            // sure that the lower VTL is executing at a known point, and only if
            // the higher VTL has not been enabled on any other VP because at that
            // point, the higher VTL should be orchestrating its own enablement.
            if self.intercepted_vtl < GuestVtl::Vtl1 {
                if vtl1.enabled_on_any_vp || vp_index != current_vp_index {
                    return Err(HvError::AccessDenied);
                }

                Some(vtl1)
            } else {
                // If handling on behalf of VTL 1, then some other VP (i.e. the
                // bsp) must have already handled EnableVpVtl. No partition-wide
                // state is changing, so no need to hold the lock
                assert!(vtl1.enabled_on_any_vp);
                None
            }
        };

        // Lock the remote vp state to make sure no other VP is trying to enable
        // VTL 1 on it.
        let mut vtl1_enabled = self
            .vp
            .cvm_partition()
            .vp_inner(vp_index)
            .vtl1_enable_called
            .lock();

        if *vtl1_enabled {
            return Err(HvError::VtlAlreadyEnabled);
        }

        let hv_vp_context = match self.vp.partition.isolation {
            virt::IsolationType::None | virt::IsolationType::Vbs => unreachable!(),
            virt::IsolationType::Snp => {
                // For VTL 1, user mode needs to explicitly register the VMSA
                // with the hypervisor via the EnableVpVtl hypercall.
                let target_cpu_index = self.vp.partition.vps[vp_index as usize].cpu_index;
                let vmsa_pfn = self.vp.partition.hcl.vtl1_vmsa_pfn(target_cpu_index);
                let sev_control = hvdef::HvX64RegisterSevControl::new()
                    .with_enable_encrypted_state(true)
                    .with_vmsa_gpa_page_number(vmsa_pfn);

                let mut hv_vp_context = hvdef::hypercall::InitialVpContextX64::new_zeroed();
                hv_vp_context.rip = sev_control.into();

                hv_vp_context
            }
            virt::IsolationType::Tdx => hvdef::hypercall::InitialVpContextX64::new_zeroed(),
        };

        // Tell the hypervisor to enable VTL 1, and register any needed state
        self.vp
            .partition
            .hcl
            .enable_vp_vtl(vp_index, vtl, hv_vp_context)?;

        // Cannot fail from here
        if let Some(mut vtl1) = gvsm_state {
            // It's valid to only set this when gvsm_state is Some (when VTL 0
            // was intercepted) only because we assert above that if VTL 1 was
            // intercepted, some vp has already enabled VTL 1 on it.
            vtl1.enabled_on_any_vp = true;
        }

        *vtl1_enabled = true;

        let enable_vp_vtl_state = VpStartEnableVtl {
            operation: InitialVpContextOperation::EnableVpVtl,
            context: *vp_context,
        };

        *self
            .vp
            .cvm_partition()
            .vp_inner(vp_index)
            .hv_start_enable_vtl_vp[vtl]
            .lock() = Some(Box::new(enable_vp_vtl_state));
        self.vp.partition.vps[vp_index as usize].wake(vtl, WakeReason::HV_START_ENABLE_VP_VTL);

        tracing::debug!(vp_index, "enabled vtl 1 on vp");

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::TranslateVirtualAddressX64
    for UhHypercallHandler<'_, '_, T, B>
{
    fn translate_virtual_address(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        control_flags: hvdef::hypercall::TranslateGvaControlFlagsX64,
        gva_page: u64,
    ) -> HvResult<hvdef::hypercall::TranslateVirtualAddressOutput> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::AccessDenied);
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err(HvError::AccessDenied);
        }

        let target_vtl = self
            .target_vtl_no_higher(
                control_flags
                    .input_vtl()
                    .target_vtl()?
                    .unwrap_or(self.intercepted_vtl.into()),
            )
            .map_err(|_| HvError::AccessDenied)?;

        if self.intercepted_vtl == target_vtl {
            return Err(HvError::AccessDenied);
        }

        let gva = gva_page * hvdef::HV_PAGE_SIZE;

        if control_flags.tlb_flush_inhibit() {
            self.vp
                .set_tlb_lock(self.intercepted_vtl.into(), target_vtl);
        }

        match virt_support_x86emu::translate::translate_gva_to_gpa(
            &self.vp.partition.gm[target_vtl],
            gva,
            &self.vp.backing.translation_registers(self.vp, target_vtl),
            virt_support_x86emu::translate::TranslateFlags::from_hv_flags(control_flags),
        ) {
            Ok(virt_support_x86emu::translate::TranslateResult { gpa, cache_info }) => {
                let cache_type = match cache_info {
                    TranslateCachingInfo::NoPaging => HvCacheType::HvCacheTypeWriteBack.0 as u8,
                    TranslateCachingInfo::Paging { pat_index } => {
                        ((self.vp.access_state(target_vtl.into()).pat().unwrap().value
                            >> (pat_index * 8))
                            & 0xff) as u8
                    }
                };

                let gpn = gpa / hvdef::HV_PAGE_SIZE;
                Ok(hvdef::hypercall::TranslateVirtualAddressOutput {
                    translation_result: hvdef::hypercall::TranslateGvaResult::new()
                        .with_result_code(TranslateGvaResultCode::SUCCESS.0)
                        .with_overlay_page(
                            self.vp
                                .cvm_partition()
                                .isolated_memory_protector
                                .is_overlay_page(self.intercepted_vtl, gpn),
                        )
                        .with_cache_type(cache_type),
                    gpa_page: gpn,
                })
            }
            Err(err) => Ok(hvdef::hypercall::TranslateVirtualAddressOutput {
                translation_result: hvdef::hypercall::TranslateGvaResult::new()
                    .with_result_code(TranslateGvaResultCode::from(err).0),
                gpa_page: 0,
            }),
        }
    }
}

pub(crate) struct CvmVtlProtectAccess<'a> {
    pub vtl: GuestVtl,
    pub protector: &'a dyn crate::ProtectIsolatedMemory,
    pub tlb_access: &'a mut dyn TlbFlushLockAccess,
    pub guest_memory: &'a GuestMemory,
}

impl hv1_emulator::VtlProtectAccess for CvmVtlProtectAccess<'_> {
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        check_perms: HvMapGpaFlags,
        new_perms: Option<HvMapGpaFlags>,
    ) -> Result<guestmem::LockedPages, HvError> {
        self.protector.register_overlay_page(
            self.vtl,
            gpn,
            GpnSource::GuestMemory,
            check_perms,
            new_perms,
            self.tlb_access,
        )?;
        // TODO: underhill_mem should really be responsible for constructing the
        // LockedPages, but that requires some refactoring. For now, we just use
        // guest memory to lock the pages. When this is cleaned up, don't forget
        // to also cleanup how underhill_mem handles locking overlay pages.
        Ok(self.guest_memory.lock_gpns(false, &[gpn]).unwrap())
    }

    fn unlock_overlay_page(&mut self, gpn: u64) -> Result<(), HvError> {
        self.protector
            .unregister_overlay_page(self.vtl, gpn, self.tlb_access)
    }
}

#[expect(private_bounds)]
impl<B: HardwareIsolatedBacking> UhProcessor<'_, B> {
    pub(crate) fn read_msr_cvm(&self, msr: u32, vtl: GuestVtl) -> Result<u64, MsrError> {
        self.backing.cvm_state().hv[vtl]
            .msr_read(msr)
            .or_else_if_unknown(|| self.read_crash_msr(msr, vtl))
    }

    pub(crate) fn write_msr_cvm(
        &mut self,
        msr: u32,
        value: u64,
        vtl: GuestVtl,
    ) -> Result<(), MsrError> {
        if let Ok(()) = self.write_crash_msr(msr, value, vtl) {
            return Ok(());
        }

        let self_index = self.vp_index();
        let hv = &mut self.backing.cvm_state_mut().hv[vtl];

        let mut access = CvmVtlProtectAccess {
            vtl,
            protector: B::cvm_partition_state(self.shared)
                .isolated_memory_protector
                .as_ref(),
            tlb_access: &mut B::tlb_flush_lock_access(
                Some(self_index),
                self.partition,
                self.shared,
            ),
            guest_memory: &self.partition.gm[vtl],
        };
        let r = hv.msr_write(msr, value, &mut access);

        // If the MSR is a synic MSR, then update the `proxy_irr_blocked`
        if vtl == GuestVtl::Vtl0
            && !matches!(r, Err(MsrError::Unknown))
            && matches!(msr, hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15)
        {
            self.update_proxy_irr_filter(vtl);
        }
        r
    }

    pub(crate) fn cvm_cpuid_result(&mut self, vtl: GuestVtl, leaf: u32, subleaf: u32) -> [u32; 4] {
        // Get the base fixed values.
        let [mut eax, mut ebx, mut ecx, mut edx] =
            self.partition.cpuid.result(leaf, subleaf, &[0, 0, 0, 0]);

        // Apply fixups. These must be runtime changes only, for parts of cpuid
        // that are dynamic (either because it's a function of the current VP's
        // identity or the current VP or partition state).
        //
        // We rely on the cpuid set being accurate during partition startup,
        // without running through this code, so violations of this principle
        // may cause the partition to be constructed improperly.
        match CpuidFunction(leaf) {
            CpuidFunction::VersionAndFeatures => {
                let cr4 = B::cr4(self, vtl);
                ecx = cpuid::VersionAndFeaturesEcx::from(ecx)
                    .with_os_xsave(cr4 & x86defs::X64_CR4_OSXSAVE != 0)
                    .into();
                ebx = cpuid::VersionAndFeaturesEbx::from(ebx)
                    .with_initial_apic_id(self.inner.vp_info.apic_id as u8)
                    .into();
            }
            CpuidFunction::ExtendedTopologyEnumeration => {
                if subleaf == 0 || subleaf == 1 {
                    edx = self.inner.vp_info.apic_id;
                }
            }
            CpuidFunction::ExtendedStateEnumeration => match subleaf {
                0 => {
                    let mut state = self.access_state(vtl.into());
                    let xfem = state.xcr().expect("can't fail to get xfem").value;
                    drop(state);
                    ebx = self.partition.caps.xsave.standard_len_for(xfem);
                }
                1 => {
                    if cpuid::ExtendedStateEnumerationSubleaf1Eax::from(eax).xsave_s() {
                        let mut state = self.access_state(vtl.into());
                        let xfem = state.xcr().expect("can't fail to get xfem").value;
                        let xss = state.xss().expect("can't fail to get xss").value;
                        drop(state);
                        ebx = self.partition.caps.xsave.compact_len_for(xfem | xss);
                    }
                }
                _ => {}
            },
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION) => {
                // If VSM has been revoked (or just isn't available) then don't
                // recommend the use of TLB flush hypercalls. They are only needed
                // for synchronization between VTLs, and the non-hypercall direct
                // path is always more efficient.
                if matches!(
                    *self.cvm_partition().guest_vsm.read(),
                    GuestVsmState::NotPlatformSupported
                ) {
                    [eax, ebx, ecx, edx] =
                        hvdef::HvEnlightenmentInformation::from_cpuid([eax, ebx, ecx, edx])
                            .with_use_hypercall_for_remote_flush_and_local_flush_entire(false)
                            .into_cpuid();
                }
            }
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES) => {
                // Update the VSM access privilege if it's been revoked by UEFI.
                if matches!(
                    *self.cvm_partition().guest_vsm.read(),
                    GuestVsmState::NotPlatformSupported
                ) {
                    let mut features = hvdef::HvFeatures::from_cpuid([eax, ebx, ecx, edx]);
                    features.set_privileges(features.privileges().with_access_vsm(false));
                    [eax, ebx, ecx, edx] = features.into_cpuid();
                }
            }

            _ => {}
        }
        [eax, ebx, ecx, edx]
    }

    fn set_vsm_partition_config(
        &mut self,
        value: HvRegisterVsmPartitionConfig,
        vtl: GuestVtl,
    ) -> HvResult<()> {
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        assert!(self.partition.isolation.is_isolated());

        // Features currently supported by openhcl.
        let allowed_bits = HvRegisterVsmPartitionConfig::new()
            .with_enable_vtl_protection(true)
            .with_default_vtl_protection_mask(0xf)
            .with_zero_memory_on_reset(true)
            .with_deny_lower_vtl_startup(true);

        if (!u64::from(allowed_bits) & u64::from(value)) != 0 {
            return Err(HvError::InvalidRegisterValue);
        }

        // VTL 1 must be enabled already.
        let mut guest_vsm_lock = self.cvm_partition().guest_vsm.write();
        let GuestVsmState::Enabled { vtl1, .. } = &mut *guest_vsm_lock else {
            return Err(HvError::InvalidVtlState);
        };

        let protections = HvMapGpaFlags::from(value.default_vtl_protection_mask() as u32);

        let protector = &self.cvm_partition().isolated_memory_protector;
        // VTL protection cannot be disabled once enabled.
        if !value.enable_vtl_protection() && protector.vtl1_protections_enabled() {
            return Err(HvError::InvalidRegisterValue);
        }

        if !validate_vtl_gpa_flags(
            protections,
            vtl1.mbec_enabled,
            vtl1.shadow_supervisor_stack_enabled,
        ) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Default VTL protection mask must include read and write.
        if !(protections.readable() && protections.writable()) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Protections given to set_vsm_partition_config actually apply to VTLs lower
        // than the VTL specified as an argument for hardware CVMs.
        let targeted_vtl = GuestVtl::Vtl0;

        // Don't allow changing existing protections once vtl protection is enabled
        if protector.vtl1_protections_enabled() {
            let current_protections = protector.default_vtl0_protections();
            if protections != current_protections {
                return Err(HvError::InvalidRegisterValue);
            }
        }

        protector.change_default_vtl_protections(
            targeted_vtl,
            protections,
            &mut self.tlb_flush_lock_access(),
        )?;

        // TODO GUEST VSM: should only be set if enable_vtl_protection is true?
        // We're not to spec but match the HCL, so good enough for now?
        protector.set_vtl1_protections_enabled();

        // Note: Zero memory on reset will happen regardless of this value,
        // since reset that involves resetting from UEFI isn't supported, and
        // the partition will get torn down and reconstructed by the host.
        vtl1.zero_memory_on_reset = value.zero_memory_on_reset();
        vtl1.deny_lower_vtl_startup = value.deny_lower_vtl_startup();

        Ok(())
    }

    /// Returns the partition-wide CVM state.
    pub(crate) fn cvm_partition(&self) -> &'_ crate::UhCvmPartitionState {
        B::cvm_partition_state(self.shared)
    }

    /// Returns the per-vp cvm inner state for this vp
    pub(crate) fn cvm_vp_inner(&self) -> &'_ crate::UhCvmVpInner {
        self.cvm_partition().vp_inner(self.vp_index().index())
    }

    /// Returns the appropriately backed TLB flush and lock access
    pub(crate) fn tlb_flush_lock_access(&self) -> impl TlbFlushLockAccess + use<'_, B> {
        B::tlb_flush_lock_access(Some(self.vp_index()), self.partition, self.shared)
    }

    /// Handle checking for cross-VTL interrupts, preempting VTL 0, and setting
    /// VINA when appropriate. Returns true if interrupt reprocessing is required.
    fn cvm_handle_cross_vtl_interrupts(&mut self, dev: &impl CpuIo) -> bool {
        let cvm_state = self.backing.cvm_state();

        // If VTL1 is not yet enabled, there is nothing to do.
        if cvm_state.vtl1.is_none() {
            return false;
        }

        // Check for VTL preemption - which ignores RFLAGS.IF
        if cvm_state.exit_vtl == GuestVtl::Vtl0
            && B::is_interrupt_pending(self, GuestVtl::Vtl1, false, dev)
        {
            self.raise_vtl(GuestVtl::Vtl0, GuestVtl::Vtl1, HvVtlEntryReason::INTERRUPT);
        }

        let mut reprocessing_required = false;

        // Check for VINA
        if self.backing.cvm_state().exit_vtl == GuestVtl::Vtl1
            && B::is_interrupt_pending(self, GuestVtl::Vtl0, true, dev)
        {
            let hv = &mut self.backing.cvm_state_mut().hv[GuestVtl::Vtl1];
            let vina = hv.synic.vina();

            if vina.enabled() && !hv.vina_asserted() {
                hv.set_vina_asserted(true);
                self.partition
                    .synic_interrupt(self.vp_index(), GuestVtl::Vtl1)
                    .request_interrupt(vina.vector().into(), vina.auto_eoi());
                reprocessing_required = true;
            }
        }

        reprocessing_required
    }

    pub(crate) fn hcvm_handle_vp_start_enable_vtl(&mut self, vtl: GuestVtl) {
        let context = {
            self.cvm_vp_inner().hv_start_enable_vtl_vp[vtl]
                .lock()
                .take()
        };
        if let Some(start_enable_vtl_state) = context {
            if vtl == GuestVtl::Vtl1 {
                assert!(*self.cvm_vp_inner().vtl1_enable_called.lock());
                if let InitialVpContextOperation::EnableVpVtl = start_enable_vtl_state.operation {
                    self.backing.cvm_state_mut().vtl1 = Some(crate::GuestVsmVpState::new());
                }
            }

            tracing::debug!(
                vp_index = self.vp_index().index(),
                ?vtl,
                ?start_enable_vtl_state.operation,
                "setting up vp with initial registers"
            );

            hv1_emulator::hypercall::set_x86_vp_context(
                &mut self.access_state(vtl.into()),
                &(start_enable_vtl_state.context),
            )
            .unwrap();

            if let InitialVpContextOperation::StartVp = start_enable_vtl_state.operation {
                match vtl {
                    GuestVtl::Vtl0 => {
                        if self.backing.cvm_state().vtl1.is_some() {
                            // When starting a VP targeting VTL on a
                            // hardware confidential VM, if VTL 1 has been
                            // enabled, switch to it (the highest enabled
                            // VTL should run first). This is largely true
                            // because startvp is disallowed on a VP that
                            // has already been started. If this is allowed
                            // in the future, whether to switch to VTL 1 on
                            // a second+ startvp call for a vp should be
                            // revisited.
                            //
                            // Furthermore, there is no need to copy the
                            // shared VTL registers if starting the VP on an
                            // already running VP is disallowed. Even if
                            // this was allowed, copying the registers may
                            // not be desirable.

                            self.backing.cvm_state_mut().exit_vtl = GuestVtl::Vtl1;
                        }
                    }
                    GuestVtl::Vtl1 => {
                        self.backing.cvm_state_mut().exit_vtl = GuestVtl::Vtl1;
                    }
                }
            }
        }
    }

    fn cvm_handle_exit_activity(&mut self) {
        let exit_vtl = self.backing.cvm_state().exit_vtl;
        if self.exit_activities[exit_vtl].pending_event() {
            self.cvm_deliver_exit_pending_event();
        }

        self.exit_activities[exit_vtl] = Default::default();
    }

    pub(crate) fn cvm_deliver_exit_pending_event(&mut self) {
        let next_vtl = self.backing.cvm_state().exit_vtl;

        // Currently, the only pending event that needs to be delivered on exit
        // are those that VTL 1 injects into VTL 0.
        if next_vtl == GuestVtl::Vtl0 {
            let pending_event = self
                .backing
                .cvm_state()
                .vtl1
                .as_ref()
                .and_then(|vtl1| vtl1.vtl0_exit_pending_event);

            if let Some(pending_event) = pending_event {
                let double_fault = {
                    if let Some(pending_event_vector) = B::pending_event_vector(self, next_vtl) {
                        if pending_event.vector()
                            > x86defs::Exception::CONTROL_PROTECTION_EXCEPTION.0 as u16
                        {
                            false
                        } else {
                            let incoming_exception =
                                x86defs::Exception(pending_event.vector() as u8);
                            let current_exception = x86defs::Exception(pending_event_vector);

                            let is_contributory_exception =
                                |exception: x86defs::Exception| -> bool {
                                    matches!(
                                        exception,
                                        x86defs::Exception::DIVIDE_ERROR
                                            | x86defs::Exception::INVALID_TSS
                                            | x86defs::Exception::SEGMENT_NOT_PRESENT
                                            | x86defs::Exception::STACK_SEGMENT_FAULT
                                            | x86defs::Exception::GENERAL_PROTECTION_FAULT
                                            | x86defs::Exception::CONTROL_PROTECTION_EXCEPTION
                                    )
                                };

                            match (current_exception, incoming_exception) {
                                (
                                    x86defs::Exception::PAGE_FAULT,
                                    x86defs::Exception::PAGE_FAULT,
                                ) => true,
                                (x86defs::Exception::PAGE_FAULT, second_exception) => {
                                    is_contributory_exception(second_exception)
                                }
                                (first_exception, second_exception) => {
                                    is_contributory_exception(first_exception)
                                        && is_contributory_exception(second_exception)
                                }
                            }
                        }
                    } else {
                        false
                    }
                };

                if double_fault {
                    let double_fault_event = HvX64PendingExceptionEvent::new()
                        .with_vector(x86defs::Exception::DOUBLE_FAULT.0 as u16)
                        .with_deliver_error_code(true)
                        .with_error_code(0);
                    B::set_pending_exception(self, next_vtl, double_fault_event);
                } else {
                    B::set_pending_exception(self, next_vtl, pending_event);
                }

                // The pending event takes precedence over any halts or idles
                // (but not halts for the TLB lock).
                self.backing.cvm_state_mut().lapics[next_vtl].activity = virt::vp::MpState::Running;

                self.backing
                    .cvm_state_mut()
                    .vtl1
                    .as_mut()
                    .unwrap()
                    .vtl0_exit_pending_event = None;
            }
        }
    }

    pub(crate) fn hcvm_vtl1_inspectable(&self) -> bool {
        self.backing.cvm_state().vtl1.is_some()
    }

    /// Returns whether a higher VTL has registered for write intercepts on the
    /// register.
    fn cvm_is_protected_register_write(
        &self,
        vtl: GuestVtl,
        reg: HvX64RegisterName,
        value: u64,
    ) -> bool {
        if vtl == GuestVtl::Vtl0 && self.backing.cvm_state().vtl1.is_some() {
            let configured_intercepts = &self
                .backing
                .cvm_state()
                .vtl1
                .as_ref()
                .unwrap()
                .reg_intercept;
            let intercept_control = configured_intercepts.intercept_control;
            return match reg {
                HvX64RegisterName::Cr0 => {
                    intercept_control.cr0_write()
                        && (B::cr0(self, vtl) ^ value) & configured_intercepts.cr0_mask != 0
                }
                HvX64RegisterName::Cr4 => {
                    intercept_control.cr4_write()
                        && (B::cr4(self, vtl) ^ value) & configured_intercepts.cr4_mask != 0
                }
                HvX64RegisterName::Xfem => intercept_control.xcr0_write(),
                HvX64RegisterName::Gdtr => intercept_control.gdtr_write(),
                HvX64RegisterName::Idtr => intercept_control.idtr_write(),
                HvX64RegisterName::Ldtr => intercept_control.ldtr_write(),
                HvX64RegisterName::Tr => intercept_control.tr_write(),
                _ => unreachable!("unexpected secure register"),
            };
        }
        false
    }

    /// Checks if a higher VTL registered for write intercepts on the register,
    /// and sends the intercept as required.
    ///
    /// If an intercept message is posted then no further processing is required.
    /// The instruction pointer should not be advanced, since the instruction
    /// pointer must continue to point to the instruction that generated the
    /// intercept.
    #[must_use]
    pub(crate) fn cvm_try_protect_secure_register_write(
        &mut self,
        vtl: GuestVtl,
        reg: HvX64RegisterName,
        value: u64,
    ) -> bool {
        let send_intercept = self.cvm_is_protected_register_write(vtl, reg, value);
        if send_intercept {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                ?vtl,
                ?reg,
                "received protected register write, sending intercept"
            );
            let message_state = B::intercept_message_state(self, vtl, false);

            self.send_intercept_message(
                GuestVtl::Vtl1,
                &crate::processor::InterceptMessageType::Register { reg, value }
                    .generate_hv_message(self.vp_index(), vtl, message_state, false),
            );
        }

        send_intercept
    }

    /// Checks if a higher VTL registered for write intercepts on the MSR, and
    /// sends the intercept as required.
    ///
    /// If an intercept message is posted then no further processing is required.
    /// The instruction pointer should not be advanced, since the instruction
    /// pointer must continue to point to the instruction that generated the
    /// intercept.
    #[must_use]
    pub(crate) fn cvm_try_protect_msr_write(&mut self, vtl: GuestVtl, msr: u32) -> bool {
        if vtl == GuestVtl::Vtl0 && self.backing.cvm_state().vtl1.is_some() {
            let configured_intercepts = self
                .backing
                .cvm_state()
                .vtl1
                .as_ref()
                .unwrap()
                .reg_intercept
                .intercept_control;

            // Note: writes to X86X_IA32_MSR_MISC_ENABLE are dropped, so don't
            // need to check the mask.

            let send_intercept = match msr {
                x86defs::X86X_MSR_LSTAR => configured_intercepts.msr_lstar_write(),
                x86defs::X86X_MSR_STAR => configured_intercepts.msr_star_write(),
                x86defs::X86X_MSR_CSTAR => configured_intercepts.msr_cstar_write(),
                x86defs::X86X_MSR_APIC_BASE => configured_intercepts.apic_base_msr_write(),
                x86defs::X86X_MSR_EFER => configured_intercepts.msr_efer_write(),
                x86defs::X86X_MSR_SYSENTER_CS => configured_intercepts.msr_sysenter_cs_write(),
                x86defs::X86X_MSR_SYSENTER_EIP => configured_intercepts.msr_sysenter_eip_write(),
                x86defs::X86X_MSR_SYSENTER_ESP => configured_intercepts.msr_sysenter_esp_write(),
                x86defs::X86X_MSR_SFMASK => configured_intercepts.msr_sfmask_write(),
                x86defs::X86X_MSR_TSC_AUX => configured_intercepts.msr_tsc_aux_write(),
                x86defs::X86X_MSR_XSS => configured_intercepts.msr_xss_write(),
                x86defs::X86X_MSR_S_CET => configured_intercepts.msr_scet_write(),
                x86defs::X86X_MSR_PL0_SSP
                | x86defs::X86X_MSR_PL1_SSP
                | x86defs::X86X_MSR_PL2_SSP => configured_intercepts.msr_pls_ssp_write(),
                x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR => {
                    configured_intercepts.msr_interrupt_ssp_table_addr_write()
                }
                _ => false,
            };

            if send_intercept {
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    ?vtl,
                    ?msr,
                    "received protected msr write, sending intercept"
                );
                let message_state = B::intercept_message_state(self, vtl, false);

                self.send_intercept_message(
                    GuestVtl::Vtl1,
                    &crate::processor::InterceptMessageType::Msr { msr }.generate_hv_message(
                        self.vp_index(),
                        vtl,
                        message_state,
                        false,
                    ),
                );

                return true;
            }
        }
        false
    }

    /// Checks if a higher VTL registered for intercepts on io port and sends
    /// the intercept as required.
    ///
    /// If an intercept message is posted then no further processing is
    /// required. The instruction pointer should not be advanced, since the
    /// instruction pointer must continue to point to the instruction that
    /// generated the intercept.
    pub(crate) fn cvm_try_protect_io_port_access(
        &mut self,
        vtl: GuestVtl,
        port_number: u16,
        is_read: bool,
        access_size: u8,
        string_access: bool,
        rep_access: bool,
    ) -> bool {
        if vtl == GuestVtl::Vtl0 {
            let send_intercept = {
                if let GuestVsmState::Enabled { vtl1 } = &*self.cvm_partition().guest_vsm.read() {
                    if is_read {
                        vtl1.io_read_intercepts[port_number as usize]
                    } else {
                        vtl1.io_write_intercepts[port_number as usize]
                    }
                } else {
                    false
                }
            };

            if send_intercept {
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    ?vtl,
                    port_number,
                    is_read,
                    access_size,
                    string_access,
                    rep_access,
                    "received protected io port access, sending intercept"
                );
                let message_state = B::intercept_message_state(self, vtl, true);

                self.send_intercept_message(
                    GuestVtl::Vtl1,
                    &crate::processor::InterceptMessageType::IoPort {
                        port_number,
                        access_size,
                        string_access,
                        rep_access,
                    }
                    .generate_hv_message(
                        self.vp_index(),
                        vtl,
                        message_state,
                        is_read,
                    ),
                );

                return true;
            }
        }

        false
    }

    fn cvm_send_synthetic_cluster_ipi(
        &mut self,
        vtl: GuestVtl,
        vector: u32,
        processors: ProcessorSet<'_>,
    ) -> HvResult<()> {
        if vtl != GuestVtl::Vtl0 {
            return Err(HvError::InvalidVtlState);
        }

        if !(16..=255).contains(&vector) {
            return Err(HvError::InvalidParameter);
        }

        for vp_index in processors {
            self.partition
                .synic_interrupt(virt::VpIndex::new(vp_index), vtl)
                .request_interrupt(vector, false)
        }
        Ok(())
    }

    fn get_vsm_vp_secure_config_vtl(
        &mut self,
        requesting_vtl: GuestVtl,
        target_vtl: GuestVtl,
    ) -> Result<HvRegisterVsmVpSecureVtlConfig, HvError> {
        if requesting_vtl <= target_vtl {
            return Err(HvError::AccessDenied);
        }

        let requesting_vtl = requesting_vtl.into();

        let guest_vsm_lock = self.cvm_partition().guest_vsm.read();
        let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
            return Err(HvError::InvalidVtlState);
        };

        let tlb_locked = self.vtls_tlb_locked.get(requesting_vtl, target_vtl);

        Ok(HvRegisterVsmVpSecureVtlConfig::new()
            .with_mbec_enabled(vtl1.mbec_enabled)
            .with_tlb_locked(tlb_locked))
    }

    fn set_vsm_vp_secure_config_vtl(
        &mut self,
        requesting_vtl: GuestVtl,
        target_vtl: GuestVtl,
        config: HvRegisterVsmVpSecureVtlConfig,
    ) -> HvResult<()> {
        tracing::debug!(
            ?requesting_vtl,
            ?target_vtl,
            "setting vsm vp secure config vtl"
        );
        if requesting_vtl <= target_vtl {
            return Err(HvError::AccessDenied);
        }

        if config.supervisor_shadow_stack_enabled() || config.hardware_hvpt_enabled() {
            return Err(HvError::InvalidRegisterValue);
        }

        let requesting_vtl = requesting_vtl.into();

        {
            let guest_vsm_lock = self.cvm_partition().guest_vsm.read();
            let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
                return Err(HvError::InvalidVtlState);
            };

            // MBEC must always be enabled or disabled partition-wide.
            if config.mbec_enabled() != vtl1.mbec_enabled {
                return Err(HvError::InvalidRegisterValue);
            }
        }

        let tlb_locked = self.vtls_tlb_locked.get(requesting_vtl, target_vtl);
        match (tlb_locked, config.tlb_locked()) {
            (true, false) => self.unlock_tlb_lock_target(requesting_vtl, target_vtl),
            (false, true) => self.set_tlb_lock(requesting_vtl, target_vtl),
            _ => (), // Nothing to do
        };

        Ok(())
    }

    fn raise_vtl(
        &mut self,
        source_vtl: GuestVtl,
        target_vtl: GuestVtl,
        entry_reason: HvVtlEntryReason,
    ) {
        assert!(source_vtl < target_vtl);
        B::switch_vtl(self, source_vtl, target_vtl);
        self.backing.cvm_state_mut().hv[target_vtl].set_return_reason(entry_reason);
    }

    fn send_intercept_message(&mut self, vtl: GuestVtl, message: &HvMessage) {
        tracing::trace!(?message, "sending intercept to {:?}", vtl);

        if let Err(e) = self.backing.cvm_state_mut().hv[vtl]
            .synic
            .post_intercept_message(
                message,
                &mut self
                    .partition
                    .synic_interrupt(self.inner.vp_info.base.vp_index, vtl),
            )
        {
            // Dropping this allows us to try to deliver any existing
            // interrupt. In the case of sending an intercept to VTL 1
            // because of VTL 0 behavior, since the VTL 0 instruction
            // pointer is not advanced, the VTL 0 guest will exit on the
            // same instruction again, providing another opportunity to
            // deliver the intercept.
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                error = &e as &dyn std::error::Error,
                ?vtl,
                ?message,
                "error sending intercept"
            );
        }
    }

    pub(crate) fn cvm_process_interrupts(
        &mut self,
        scan_irr: VtlArray<bool, 2>,
        first_scan_irr: &mut bool,
        dev: &impl CpuIo,
    ) -> bool {
        // Cancel any existing deadline before processing interrupts.
        B::clear_deadline(self);

        self.cvm_handle_exit_activity();

        if self.backing.untrusted_synic_mut().is_some() {
            self.update_synic(GuestVtl::Vtl0, true);
        }

        for vtl in [GuestVtl::Vtl1, GuestVtl::Vtl0] {
            // Process interrupts.
            self.update_synic(vtl, false);

            B::poll_apic(self, vtl, scan_irr[vtl] || *first_scan_irr);
        }
        *first_scan_irr = false;

        self.cvm_handle_cross_vtl_interrupts(dev)
    }

    fn update_synic(&mut self, vtl: GuestVtl, untrusted_synic: bool) {
        loop {
            let hv = &mut self.backing.cvm_state_mut().hv[vtl];

            let ref_time_now = hv.ref_time_now();
            let synic = if untrusted_synic {
                debug_assert_eq!(vtl, GuestVtl::Vtl0);
                self.backing.untrusted_synic_mut().unwrap()
            } else {
                &mut hv.synic
            };
            let (ready_sints, next_ref_time) = synic.scan(
                ref_time_now,
                &mut self
                    .partition
                    .synic_interrupt(self.inner.vp_info.base.vp_index, vtl),
            );
            if let Some(next_ref_time) = next_ref_time {
                B::update_deadline(self, ref_time_now, next_ref_time);
            }
            if ready_sints == 0 {
                break;
            }
            self.deliver_synic_messages(vtl, ready_sints);
            // Loop around to process the synic again.
        }
    }

    pub(crate) fn deliver_synic_messages(&mut self, vtl: GuestVtl, sints: u16) {
        let proxied_sints = self.backing.cvm_state().hv[vtl].synic.proxied_sints();
        let pending_sints =
            self.inner.message_queues[vtl].post_pending_messages(sints, |sint, message| {
                if proxied_sints & (1 << sint) != 0 {
                    if let Some(synic) = self.backing.untrusted_synic_mut() {
                        synic.post_message(
                            sint,
                            message,
                            &mut self
                                .partition
                                .synic_interrupt(self.inner.vp_info.base.vp_index, vtl),
                        )
                    } else {
                        self.partition.hcl.post_message_direct(
                            self.inner.vp_info.base.vp_index.index(),
                            sint,
                            message,
                        )
                    }
                } else {
                    self.backing.cvm_state_mut().hv[vtl].synic.post_message(
                        sint,
                        message,
                        &mut self
                            .partition
                            .synic_interrupt(self.inner.vp_info.base.vp_index, vtl),
                    )
                }
            });

        self.request_sint_notifications(vtl, pending_sints);
    }

    /// Checks if a memory fault for the given VTL and GPA should be emulated,
    /// or otherwise handled. Returns true if emulation is required, false if
    /// all the necessary work is now done.
    pub(crate) fn check_mem_fault(
        &mut self,
        vtl: GuestVtl,
        gpa: u64,
        is_write: bool,
        extra_info: impl std::fmt::Debug,
    ) -> bool {
        let vtom = self.partition.caps.vtom.unwrap_or(0);
        let is_shared = (gpa & vtom) == vtom && vtom != 0;
        let canonical_gpa = gpa & !vtom;

        // Only emulate the access if the gpa is mmio or outside of ram.
        let address_type = self
            .partition
            .lower_vtl_memory_layout
            .probe_address(canonical_gpa);

        match address_type {
            Some(AddressType::Mmio) => {
                // Emulate the access.
                true
            }
            Some(AddressType::Ram) => {
                let (access_check, access_type) = if is_write {
                    (
                        self.partition.gm[vtl].probe_gpa_writable(gpa),
                        HvInterceptAccessType::WRITE,
                    )
                } else {
                    (
                        self.partition.gm[vtl].probe_gpa_readable(gpa),
                        HvInterceptAccessType::READ,
                    )
                };

                match access_check {
                    Ok(()) => {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            gpa,
                            ?vtl,
                            ?extra_info,
                            ?access_type,
                            "possible spurious memory access violation, ignoring"
                        );
                    }
                    Err(GuestMemoryErrorKind::VtlProtected) if vtl == GuestVtl::Vtl0 => {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            gpa,
                            ?vtl,
                            ?extra_info,
                            ?access_type,
                            "guest accessed protected gpa, sending intercept"
                        );
                        let state = B::intercept_message_state(self, vtl, false);
                        // TODO: We may want to fill in tpr_priority and gva
                        // but tests pass without them.
                        self.send_intercept_message(
                            GuestVtl::Vtl1,
                            &HvMessage::new(
                                HvMessageType::HvMessageTypeGpaIntercept,
                                0,
                                HvX64MemoryInterceptMessage {
                                    header: HvX64InterceptMessageHeader {
                                        vp_index: self.vp_index().index(),
                                        instruction_length_and_cr8: state
                                            .instruction_length_and_cr8,
                                        intercept_access_type: access_type,
                                        execution_state: hvdef::HvX64VpExecutionState::new()
                                            .with_cpl(state.cpl)
                                            .with_vtl(vtl.into())
                                            .with_efer_lma(state.efer_lma),
                                        cs_segment: state.cs,
                                        rip: state.rip,
                                        rflags: state.rflags,
                                    },
                                    cache_type: HvCacheType::HvCacheTypeWriteBack,
                                    memory_access_info: HvX64MemoryAccessInfo::new(),
                                    tpr_priority: 0,
                                    reserved: 0,
                                    guest_virtual_address: 0,
                                    guest_physical_address: gpa,
                                    instruction_byte_count: 0,
                                    instruction_bytes: [0; 16],
                                }
                                .as_bytes(),
                            ),
                        );
                    }
                    // TODO: Handle other error kinds differently?
                    Err(_) => {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            gpa,
                            ?vtl,
                            is_shared,
                            ?extra_info,
                            ?access_type,
                            "guest accessed inaccessible gpa, injecting MC"
                        );
                        // TODO: Implement IA32_MCG_STATUS MSR for more reporting
                        B::set_pending_exception(
                            self,
                            vtl,
                            HvX64PendingExceptionEvent::new()
                                .with_vector(x86defs::Exception::MACHINE_CHECK.0 as u16),
                        );
                    }
                }
                false
            }
            None => {
                if self.partition.monitor_page.gpa() == Some(gpa & !(hvdef::HV_PAGE_SIZE - 1)) {
                    if !is_write {
                        tracing::debug!(
                            CVM_ALLOWED,
                            gpa,
                            ?vtl,
                            is_shared,
                            ?extra_info,
                            "spurious exit for guest monitor page read"
                        );
                    }

                    // Emulate monitor page writes, but not reads.
                    is_write
                } else {
                    if !self.cvm_partition().hide_isolation {
                        // TODO: Addresses outside of ram and mmio probably should
                        // not be accessed by the guest, if it has been told about
                        // isolation. While it's okay as we will return FFs or
                        // discard writes for addresses that are not mmio, we should
                        // consider if instead we should also inject a machine check
                        // for such accesses. The guest should not access any
                        // addresses not described to it.
                        //
                        // For now, log that the guest did this.
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            gpa,
                            ?vtl,
                            is_shared,
                            ?extra_info,
                            "guest accessed gpa not described in memory layout, emulating anyways"
                        );
                    }

                    // Emulate the access.
                    true
                }
            }
        }
    }
}

pub(crate) struct XsetbvExitInput {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub cr4: u64,
    pub cpl: u8,
}

/// Validates registers are in the correct states during a xsetbv exit, and return
/// the new xfem value if everything's valid.
pub(crate) fn validate_xsetbv_exit(input: XsetbvExitInput) -> Option<u64> {
    let XsetbvExitInput {
        rax,
        rcx,
        rdx,
        cr4,
        cpl,
    } = input;

    if rcx != 0 {
        tracelimit::warn_ratelimited!(CVM_ALLOWED, rcx, "xsetbv exit: rcx is not set to 0");
        return None;
    }

    if cpl != 0 {
        tracelimit::warn_ratelimited!(CVM_ALLOWED, cpl, "xsetbv exit: invalid cpl");
        return None;
    }

    let osxsave_flag = cr4 & x86defs::X64_CR4_OSXSAVE;
    if osxsave_flag == 0 {
        tracelimit::warn_ratelimited!(CVM_ALLOWED, cr4, "xsetbv exit: cr4 osxsave not set");
        return None;
    }

    let xfem = (rdx << 32) | (rax & 0xffffffff);

    if (xfem & x86defs::xsave::XFEATURE_X87) == 0 {
        tracelimit::warn_ratelimited!(
            CVM_ALLOWED,
            xfem,
            "xsetbv exit: xfem legacy x87 bit not set"
        );
        return None;
    }

    Some(xfem)
}

impl<T: CpuIo, B: HardwareIsolatedBacking> TranslateGvaSupport for UhEmulationState<'_, '_, T, B> {
    fn guest_memory(&self) -> &GuestMemory {
        &self.vp.partition.gm[self.vtl]
    }

    fn acquire_tlb_lock(&mut self) {
        self.vp.set_tlb_lock(Vtl::Vtl2, self.vtl)
    }

    fn registers(&mut self) -> TranslationRegisters {
        self.vp.backing.translation_registers(self.vp, self.vtl)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::SendSyntheticClusterIpi
    for UhHypercallHandler<'_, '_, T, B>
{
    fn send_synthetic_cluster_ipi(
        &mut self,
        target_vtl: Option<Vtl>,
        vector: u32,
        flags: u8,
        processor_set: ProcessorSet<'_>,
    ) -> HvResult<()> {
        if flags != 0 {
            return Err(HvError::InvalidParameter);
        }

        let target_vtl =
            self.target_vtl_no_higher(target_vtl.unwrap_or_else(|| self.intercepted_vtl.into()))?;

        self.vp
            .cvm_send_synthetic_cluster_ipi(target_vtl, vector, processor_set)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::SendSyntheticClusterIpiEx
    for UhHypercallHandler<'_, '_, T, B>
{
    fn send_synthetic_cluster_ipi_ex(
        &mut self,
        target_vtl: Option<Vtl>,
        vector: u32,
        flags: u8,
        processor_set: ProcessorSet<'_>,
    ) -> HvResult<()> {
        if flags != 0 {
            return Err(HvError::InvalidParameter);
        }

        let target_vtl =
            self.target_vtl_no_higher(target_vtl.unwrap_or_else(|| self.intercepted_vtl.into()))?;

        self.vp
            .cvm_send_synthetic_cluster_ipi(target_vtl, vector, processor_set)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::InstallIntercept
    for UhHypercallHandler<'_, '_, T, B>
{
    fn install_intercept(
        &mut self,
        partition_id: u64,
        access_type_mask: u32,
        intercept_type: hvdef::hypercall::HvInterceptType,
        intercept_parameters: hvdef::hypercall::HvInterceptParameters,
    ) -> HvResult<()> {
        tracing::debug!(
            vp_index = self.vp.vp_index().index(),
            ?access_type_mask,
            ?intercept_type,
            ?intercept_parameters,
            "HvInstallIntercept"
        );

        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::AccessDenied);
        }

        if self.intercepted_vtl == GuestVtl::Vtl0 {
            return Err(HvError::AccessDenied);
        }

        match intercept_type {
            hvdef::hypercall::HvInterceptType::HvInterceptTypeX64IoPort => {
                if access_type_mask
                    & !(HV_INTERCEPT_ACCESS_MASK_READ | HV_INTERCEPT_ACCESS_MASK_WRITE)
                    != 0
                {
                    return Err(HvError::InvalidParameter);
                }

                let mut gvsm_lock = self.vp.cvm_partition().guest_vsm.write();

                let GuestVsmState::Enabled { vtl1, .. } = &mut *gvsm_lock else {
                    return Err(HvError::InvalidVtlState);
                };

                let io_port = intercept_parameters.io_port() as usize;

                vtl1.io_read_intercepts.set(
                    io_port,
                    access_type_mask & HV_INTERCEPT_ACCESS_MASK_READ != 0,
                );

                vtl1.io_write_intercepts.set(
                    io_port,
                    access_type_mask & HV_INTERCEPT_ACCESS_MASK_WRITE != 0,
                );

                // TODO GUEST VSM: flush io port accesses on other VPs before
                // returning back to VTL 0
            }
            _ => return Err(HvError::InvalidParameter),
        }

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::AssertVirtualInterrupt
    for UhHypercallHandler<'_, '_, T, B>
{
    fn assert_virtual_interrupt(
        &mut self,
        partition_id: u64,
        interrupt_control: hvdef::HvInterruptControl,
        destination_address: u64,
        requested_vector: u32,
        target_vtl: Vtl,
    ) -> HvResult<()> {
        let target_vtl = self.target_vtl_no_higher(target_vtl)?;

        if partition_id != hvdef::HV_PARTITION_ID_SELF || target_vtl == self.intercepted_vtl {
            return Err(HvError::AccessDenied);
        }

        // Only fixed interrupts and NMIs are supported today.
        if !matches!(
            interrupt_control.interrupt_type(),
            HvInterruptType::HvX64InterruptTypeFixed | HvInterruptType::HvX64InterruptTypeNmi
        ) {
            return Err(HvError::InvalidParameter);
        }

        self.vp.partition.request_msi(
            target_vtl,
            MsiRequest::new_x86(
                x86defs::apic::DeliveryMode(
                    interrupt_control
                        .interrupt_type()
                        .0
                        .try_into()
                        .map_err(|_| HvError::InvalidParameter)?,
                ),
                destination_address
                    .try_into()
                    .map_err(|_| HvError::InvalidParameter)?,
                interrupt_control.x86_logical_destination_mode(),
                requested_vector
                    .try_into()
                    .map_err(|_| HvError::InvalidParameter)?,
                interrupt_control.x86_level_triggered(),
            ),
        );

        Ok(())
    }
}

/// Trait for managing lower VTL timer deadline in hardware-isolated partitions.
///
/// Note that this interface is currently used only for synic timer emulation in VTL2
/// and not for APIC timers. APIC timer emulation uses [`VmTime`] directly for managing
/// its timer deadlines. In practice, VTL0 guest kernels typically prefer Hyper-V
/// synthetic timers over APIC timers, so this should not be a concern. This can be
/// revisited in the future if APIC timer emulation performance becomes a priority.
pub(super) trait HardwareIsolatedGuestTimer<T: HardwareIsolatedBacking>:
    Send + Sync
{
    /// Returns true if the implementation uses hardware virtualized timer service.
    fn is_hardware_virtualized(&self) -> bool;

    /// Update timer deadline.
    fn update_deadline(&self, vp: &mut UhProcessor<'_, T>, ref_time_now: u64, ref_time_next: u64);

    /// Clear any pending deadline.
    fn clear_deadline(&self, vp: &mut UhProcessor<'_, T>);

    /// Synchronize armed deadline state for hardware virtualized timers.
    fn sync_deadline_state(&self, vp: &mut UhProcessor<'_, T>);
}

/// Interface for managing lower VTL timer deadlines via [`VmTime`].
/// This is the default interface used when a hardware-isolated backing doesn't support
/// timer virtualization.
pub(super) struct VmTimeGuestTimer;

impl<T: HardwareIsolatedBacking> HardwareIsolatedGuestTimer<T> for VmTimeGuestTimer {
    fn is_hardware_virtualized(&self) -> bool {
        false
    }

    /// Update timer deadline.
    fn update_deadline(&self, vp: &mut UhProcessor<'_, T>, ref_time_now: u64, ref_time_next: u64) {
        /// Convert reference time in 100ns units to Duration.
        fn duration_from_100ns(n: u64) -> std::time::Duration {
            const NUM_100NS_IN_SEC: u64 = 10 * 1000 * 1000;
            std::time::Duration::new(n / NUM_100NS_IN_SEC, (n % NUM_100NS_IN_SEC) as u32 * 100)
        }

        // Convert from reference timer basis to [`VmTime`] basis via
        // difference of programmed timer and current reference time.
        let ref_diff = ref_time_next.saturating_sub(ref_time_now);
        let timeout = vp.vmtime.now().wrapping_add(duration_from_100ns(ref_diff));
        vp.vmtime.set_timeout_if_before(timeout);
    }

    /// Clear any pending deadline.
    fn clear_deadline(&self, vp: &mut UhProcessor<'_, T>) {
        vp.vmtime.cancel_timeout();
    }

    fn sync_deadline_state(&self, _vp: &mut UhProcessor<'_, T>) {
        // No-op for software timers
    }
}
