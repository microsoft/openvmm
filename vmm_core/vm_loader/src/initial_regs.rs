// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Logic for generating the initial register set for a VM.

#[cfg(guest_arch = "aarch64")]
pub use aarch64_initial_regs as initial_regs;
#[cfg(guest_arch = "x86_64")]
pub use x86_initial_regs as initial_regs;

use loader::importer::Aarch64Register;
use loader::importer::X86Register;
use std::sync::Arc;
use vm_topology::processor::aarch64::Aarch64VpInfo;
use vm_topology::processor::x86::X86VpInfo;

/// Converts a list of loader registers to the VM initial register state.
pub fn x86_initial_regs(
    init: &[X86Register],
    caps: &virt::x86::X86PartitionCapabilities,
    bsp_id: &X86VpInfo,
) -> Arc<virt::x86::X86InitialRegs> {
    let mut regs = Arc::new(virt::x86::X86InitialRegs::at_reset(caps, bsp_id));

    let state = Arc::get_mut(&mut regs).unwrap();
    for &reg in init {
        match reg {
            X86Register::Gdtr(v) => state.registers.gdtr = into_virt_x86_tr(v),
            X86Register::Idtr(v) => state.registers.idtr = into_virt_x86_tr(v),
            X86Register::Ds(v) => state.registers.ds = into_virt_x86_sr(v),
            X86Register::Es(v) => state.registers.es = into_virt_x86_sr(v),
            X86Register::Fs(v) => state.registers.fs = into_virt_x86_sr(v),
            X86Register::Gs(v) => state.registers.gs = into_virt_x86_sr(v),
            X86Register::Ss(v) => state.registers.ss = into_virt_x86_sr(v),
            X86Register::Cs(v) => state.registers.cs = into_virt_x86_sr(v),
            X86Register::Tr(v) => state.registers.tr = into_virt_x86_sr(v),
            X86Register::Cr0(v) => state.registers.cr0 = v,
            X86Register::Cr3(v) => state.registers.cr3 = v,
            X86Register::Cr4(v) => state.registers.cr4 = v,
            X86Register::Efer(v) => state.registers.efer = v,
            X86Register::Pat(v) => state.pat.value = v,
            X86Register::Rbp(v) => state.registers.rbp = v,
            X86Register::Rip(v) => state.registers.rip = v,
            X86Register::Rsi(v) => state.registers.rsi = v,
            X86Register::Rsp(v) => state.registers.rsp = v,
            X86Register::R8(v) => state.registers.r8 = v,
            X86Register::R9(v) => state.registers.r9 = v,
            X86Register::R10(v) => state.registers.r10 = v,
            X86Register::R11(v) => state.registers.r11 = v,
            X86Register::R12(v) => state.registers.r12 = v,
            X86Register::Rflags(v) => state.registers.rflags = v,
            X86Register::MtrrDefType(v) => state.cc.msr_mtrr_def_type = v,
            X86Register::MtrrPhysBase0(v) => state.cc.variable[0] = v,
            X86Register::MtrrPhysMask0(v) => state.cc.variable[1] = v,
            X86Register::MtrrPhysBase1(v) => state.cc.variable[2] = v,
            X86Register::MtrrPhysMask1(v) => state.cc.variable[3] = v,
            X86Register::MtrrPhysBase2(v) => state.cc.variable[4] = v,
            X86Register::MtrrPhysMask2(v) => state.cc.variable[5] = v,
            X86Register::MtrrPhysBase3(v) => state.cc.variable[6] = v,
            X86Register::MtrrPhysMask3(v) => state.cc.variable[7] = v,
            X86Register::MtrrPhysBase4(v) => state.cc.variable[8] = v,
            X86Register::MtrrPhysMask4(v) => state.cc.variable[9] = v,
            X86Register::MtrrFix64k00000(v) => state.cc.fixed[0] = v,
            X86Register::MtrrFix16k80000(v) => state.cc.fixed[1] = v,
            X86Register::MtrrFix4kE0000(v) => state.cc.fixed[7] = v,
            X86Register::MtrrFix4kE8000(v) => state.cc.fixed[8] = v,
            X86Register::MtrrFix4kF0000(v) => state.cc.fixed[9] = v,
            X86Register::MtrrFix4kF8000(v) => state.cc.fixed[10] = v,
        }
    }

    regs
}

fn into_virt_x86_tr(tr: loader::importer::TableRegister) -> virt::x86::TableRegister {
    virt::x86::TableRegister {
        base: tr.base,
        limit: tr.limit,
    }
}

fn into_virt_x86_sr(sr: loader::importer::SegmentRegister) -> virt::x86::SegmentRegister {
    virt::x86::SegmentRegister {
        base: sr.base,
        limit: sr.limit,
        selector: sr.selector,
        attributes: sr.attributes,
    }
}

/// Converts a list of loader registers to the VM initial register state.
pub fn aarch64_initial_regs(
    init: &[Aarch64Register],
    caps: &virt::aarch64::Aarch64PartitionCapabilities,
    bsp_id: &Aarch64VpInfo,
) -> Arc<virt::aarch64::Aarch64InitialRegs> {
    let mut regs = Arc::new(virt::aarch64::Aarch64InitialRegs::at_reset(caps, bsp_id));

    let state = Arc::get_mut(&mut regs).unwrap();
    for &reg in init {
        match reg {
            Aarch64Register::Pc(v) => state.registers.pc = v,
            Aarch64Register::X0(v) => state.registers.x0 = v,
            Aarch64Register::X1(v) => state.registers.x1 = v,
            Aarch64Register::Cpsr(v) => state.registers.cpsr = v,
            Aarch64Register::Ttbr0El1(v) => state.system_registers.ttbr0_el1 = v,
            Aarch64Register::MairEl1(v) => state.system_registers.mair_el1 = v,
            Aarch64Register::SctlrEl1(v) => state.system_registers.sctlr_el1 = v,
            Aarch64Register::TcrEl1(v) => state.system_registers.tcr_el1 = v,
            Aarch64Register::VbarEl1(v) => state.system_registers.vbar_el1 = v,
            Aarch64Register::Ttbr1El1(v) => state.system_registers.ttbr1_el1 = v,
        }
    }

    regs
}
