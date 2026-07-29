// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SEV-SNP initial VMSA conversion.
//!
//! [`vmsa_from_initial_regs`] is used by the shared direct-boot loader for SNP
//! backends, including KVM and MSHV. [`state_from_vmsa`] is currently used by
//! KVM to apply the loader-built VMSA through KVM's register-based launch path;
//! MSHV imports the VMSA page directly.

use super::SegmentRegister;
use super::TableRegister;
use super::X86InitialRegs;
use super::vp;
use thiserror::Error;
use x86defs::SegmentAttributes;
use x86defs::snp::SevSelector;
use x86defs::snp::SevVmsa;
use zerocopy::FromZeros;

const _: () = assert!(size_of::<SevVmsa>() <= hvdef::HV_PAGE_SIZE as usize);

/// The subset of initial VP state represented by a direct-boot SNP VMSA.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SnpVmsaState {
    /// Architectural register state.
    pub registers: vp::Registers,
    /// Page attribute table state.
    pub pat: u64,
    /// Extended control register 0.
    pub xcr0: u64,
    /// Whether restricted interrupt injection is enabled.
    pub restricted_injection: bool,
}

/// An error parsing a direct-boot SNP VMSA.
#[derive(Debug, Error)]
pub enum SnpVmsaError {
    /// The VMSA contains state outside the supported direct-boot contract.
    #[error("unsupported SNP VMSA state")]
    UnsupportedState,
}

/// Configuration for a direct-boot SNP VMSA.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SnpVmsaConfig {
    /// Enables restricted interrupt injection.
    pub restricted_injection: bool,
}

/// Builds the direct-boot SNP VMSA corresponding to `initial`.
pub fn vmsa_from_initial_regs(initial: &X86InitialRegs, config: SnpVmsaConfig) -> SevVmsa {
    vmsa_from_state(&SnpVmsaState {
        registers: initial.registers,
        pat: initial.pat.value,
        xcr0: x86defs::xsave::XFEATURE_X87,
        restricted_injection: config.restricted_injection,
    })
}

/// Parses and validates a direct-boot SNP VMSA.
pub fn state_from_vmsa(vmsa: &SevVmsa) -> Result<SnpVmsaState, SnpVmsaError> {
    let state = SnpVmsaState {
        registers: vp::Registers {
            rax: vmsa.rax,
            rcx: vmsa.rcx,
            rdx: vmsa.rdx,
            rbx: vmsa.rbx,
            rbp: vmsa.rbp,
            rsp: vmsa.rsp,
            rsi: vmsa.rsi,
            rdi: vmsa.rdi,
            r8: vmsa.r8,
            r9: vmsa.r9,
            r10: vmsa.r10,
            r11: vmsa.r11,
            r12: vmsa.r12,
            r13: vmsa.r13,
            r14: vmsa.r14,
            r15: vmsa.r15,
            rip: vmsa.rip,
            rflags: vmsa.rflags,
            cs: segment_from_vmsa(vmsa.cs),
            ds: segment_from_vmsa(vmsa.ds),
            es: segment_from_vmsa(vmsa.es),
            fs: segment_from_vmsa(vmsa.fs),
            gs: segment_from_vmsa(vmsa.gs),
            ss: segment_from_vmsa(vmsa.ss),
            tr: segment_from_vmsa(vmsa.tr),
            ldtr: segment_from_vmsa(vmsa.ldtr),
            gdtr: table_from_vmsa(vmsa.gdtr),
            idtr: table_from_vmsa(vmsa.idtr),
            cr0: vmsa.cr0,
            cr2: vmsa.cr2,
            cr3: vmsa.cr3,
            cr4: vmsa.cr4,
            cr8: 0,
            efer: vmsa.efer & !x86defs::X64_EFER_SVME,
        },
        pat: vmsa.pat,
        xcr0: vmsa.xcr0,
        restricted_injection: vmsa.sev_features.restrict_injection(),
    };

    if &vmsa_from_state(&state) != vmsa {
        return Err(SnpVmsaError::UnsupportedState);
    }

    Ok(state)
}

fn vmsa_from_state(state: &SnpVmsaState) -> SevVmsa {
    let registers = &state.registers;
    let mut vmsa = SevVmsa::new_zeroed();

    vmsa.es = segment_to_vmsa(registers.es);
    vmsa.cs = segment_to_vmsa(registers.cs);
    vmsa.ss = segment_to_vmsa(registers.ss);
    vmsa.ds = segment_to_vmsa(registers.ds);
    vmsa.fs = segment_to_vmsa(registers.fs);
    vmsa.gs = segment_to_vmsa(registers.gs);
    vmsa.gdtr = table_to_vmsa(registers.gdtr);
    vmsa.ldtr = segment_to_vmsa(registers.ldtr);
    vmsa.idtr = table_to_vmsa(registers.idtr);
    vmsa.tr = segment_to_vmsa(registers.tr);
    vmsa.cpl = SegmentAttributes::from(registers.cs.attributes).descriptor_privilege_level();
    vmsa.efer = registers.efer | x86defs::X64_EFER_SVME;
    vmsa.cr4 = registers.cr4;
    vmsa.cr3 = registers.cr3;
    vmsa.cr0 = registers.cr0;
    vmsa.rflags = registers.rflags;
    vmsa.rip = registers.rip;
    vmsa.rsp = registers.rsp;
    vmsa.rax = registers.rax;
    vmsa.cr2 = registers.cr2;
    vmsa.pat = state.pat;
    vmsa.rcx = registers.rcx;
    vmsa.rdx = registers.rdx;
    vmsa.rbx = registers.rbx;
    vmsa.rbp = registers.rbp;
    vmsa.rsi = registers.rsi;
    vmsa.rdi = registers.rdi;
    vmsa.r8 = registers.r8;
    vmsa.r9 = registers.r9;
    vmsa.r10 = registers.r10;
    vmsa.r11 = registers.r11;
    vmsa.r12 = registers.r12;
    vmsa.r13 = registers.r13;
    vmsa.r14 = registers.r14;
    vmsa.r15 = registers.r15;
    vmsa.sev_features.set_snp(true);
    vmsa.sev_features
        .set_restrict_injection(state.restricted_injection);
    vmsa.xcr0 = state.xcr0;

    vmsa
}

fn segment_to_vmsa(register: SegmentRegister) -> SevSelector {
    SevSelector {
        selector: register.selector,
        attrib: (register.attributes & 0xff) | ((register.attributes >> 4) & 0xf00),
        limit: register.limit,
        base: register.base,
    }
}

fn segment_from_vmsa(selector: SevSelector) -> SegmentRegister {
    SegmentRegister {
        selector: selector.selector,
        attributes: (selector.attrib & 0xff) | ((selector.attrib & 0xf00) << 4),
        limit: selector.limit,
        base: selector.base,
    }
}

fn table_to_vmsa(register: TableRegister) -> SevSelector {
    SevSelector {
        selector: 0,
        attrib: 0,
        limit: register.limit as u32,
        base: register.base,
    }
}

fn table_from_vmsa(selector: SevSelector) -> TableRegister {
    TableRegister {
        limit: selector.limit as u16,
        base: selector.base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_boot_vmsa_round_trips() {
        let initial = X86InitialRegs {
            registers: vp::Registers {
                rax: 1,
                rdx: 2,
                rip: 0x100000,
                rflags: 2,
                cs: SegmentRegister {
                    selector: 0x10,
                    attributes: 0xa09b,
                    limit: u32::MAX,
                    base: 0,
                },
                efer: x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA,
                ..Default::default()
            },
            mtrrs: Default::default(),
            pat: vp::Pat {
                value: 0x7040600070406,
            },
        };

        let vmsa = vmsa_from_initial_regs(&initial, SnpVmsaConfig::default());
        let state = state_from_vmsa(&vmsa).unwrap();

        assert_eq!(state.registers, initial.registers);
        assert_eq!(state.pat, initial.pat.value);
        assert_eq!(state.xcr0, x86defs::xsave::XFEATURE_X87);
        assert!(vmsa.sev_features.snp());
        assert_ne!(vmsa.efer & x86defs::X64_EFER_SVME, 0);
    }

    #[test]
    fn rejects_unsupported_state() {
        let initial = X86InitialRegs {
            registers: Default::default(),
            mtrrs: Default::default(),
            pat: Default::default(),
        };
        let mut vmsa = vmsa_from_initial_regs(&initial, SnpVmsaConfig::default());
        vmsa.virtual_tom = 0x1000;

        assert!(matches!(
            state_from_vmsa(&vmsa),
            Err(SnpVmsaError::UnsupportedState)
        ));
    }

    #[test]
    fn enables_restricted_injection() {
        let initial = X86InitialRegs {
            registers: Default::default(),
            mtrrs: Default::default(),
            pat: Default::default(),
        };

        let vmsa = vmsa_from_initial_regs(
            &initial,
            SnpVmsaConfig {
                restricted_injection: true,
            },
        );

        assert!(vmsa.sev_features.restrict_injection());
        assert!(state_from_vmsa(&vmsa).unwrap().restricted_injection);
    }
}
