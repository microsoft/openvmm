// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition state blob builder.
//!
//! Constructs the chunk stream that goes into the
//! `/savedstate/savedVM/partition_state` key in a `.vmrs` file. The format
//! matches the hypervisor's save/restore chunk stream as parsed by
//! `VmSavedStateDumpProvider.dll`.

use hvdef::save_restore::*;
use hvdef::AlignedU128;
use hvdef::HvX64SegmentRegister;
use hvdef::HvX64TableRegister;
use std::mem::size_of;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Processor architecture for the saved state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcessorArch {
    /// x86-64 / AMD64
    X64,
    /// ARM64 / AArch64
    Aarch64,
}

/// x64 VP register state for dump generation.
///
/// Field names and layout match the save/restore chunk ordering. Callers
/// populate this from `debug_rpc::X86VpState` or equivalent.
#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct X64VpRegisters {
    // General purpose
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    // Control
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    // Segments
    pub es: HvX64SegmentRegister,
    pub cs: HvX64SegmentRegister,
    pub ss: HvX64SegmentRegister,
    pub ds: HvX64SegmentRegister,
    pub fs: HvX64SegmentRegister,
    pub gs: HvX64SegmentRegister,
    pub ldtr: HvX64SegmentRegister,
    pub tr: HvX64SegmentRegister,
    pub cpl: u8,
    // Table registers
    pub idtr: HvX64TableRegister,
    pub gdtr: HvX64TableRegister,
    // Debug
    pub dr0: u64,
    pub dr1: u64,
    pub dr2: u64,
    pub dr3: u64,
    pub dr6: u64,
    pub dr7: u64,
    // Floating point
    pub xmm: [AlignedU128; 16],
    pub fp_mmx: [AlignedU128; 8],
    pub fp_control_status: AlignedU128,
    pub xmm_control_status: AlignedU128,
}

impl Default for X64VpRegisters {
    fn default() -> Self {
        Self {
            rax: 0, rcx: 0, rdx: 0, rbx: 0, rsp: 0, rbp: 0, rsi: 0, rdi: 0,
            r8: 0, r9: 0, r10: 0, r11: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            rip: 0, rflags: 0,
            cr0: 0, cr2: 0, cr3: 0, cr4: 0, cr8: 0, efer: 0,
            es: FromZeros::new_zeroed(), cs: FromZeros::new_zeroed(),
            ss: FromZeros::new_zeroed(), ds: FromZeros::new_zeroed(),
            fs: FromZeros::new_zeroed(), gs: FromZeros::new_zeroed(),
            ldtr: FromZeros::new_zeroed(), tr: FromZeros::new_zeroed(),
            cpl: 0,
            idtr: FromZeros::new_zeroed(), gdtr: FromZeros::new_zeroed(),
            dr0: 0, dr1: 0, dr2: 0, dr3: 0, dr6: 0, dr7: 0,
            xmm: FromZeros::new_zeroed(), fp_mmx: FromZeros::new_zeroed(),
            fp_control_status: FromZeros::new_zeroed(),
            xmm_control_status: FromZeros::new_zeroed(),
        }
    }
}

/// ARM64 VP register state for dump generation.
#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct Aarch64VpRegisters {
    // General purpose
    pub x: [u64; 29],
    pub x_fp: u64,
    pub x_lr: u64,
    pub elr_el2: u64,
    pub spsr_el2: u64,
    pub esr_el1: u64,
    pub spsr_el1: u64,
    pub far_el1: u64,
    pub par_el1: u64,
    pub elr_el1: u64,
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub afsr0_el1: u64,
    pub afsr1_el1: u64,
    // Control
    pub vmpidr_el2: u64,
    pub vpidr_el2: u64,
    pub sctlr_el1: u64,
    pub actlr_el1: u64,
    pub tcr_el1: u64,
    pub mair_el1: u64,
    pub tpidr_el1: u64,
    pub amair_el1: u64,
    pub tpidrro_el0: u64,
    pub tpidr_el0: u64,
    pub contextidr_el1: u64,
    pub cpacr_el1: u64,
    pub csselr_el1: u64,
    pub cntk_ctl_el1: u64,
    pub cntv_cval_el0: u64,
    pub cntv_ctl_el0: u64,
    // Table
    pub ttbr0_el1: u64,
    pub ttbr1_el1: u64,
    pub vbar_el1: u64,
    // Floating point / SIMD
    pub q: [AlignedU128; 32],
    pub fpsr: u64,
    pub fpcr: u64,
}

impl Default for Aarch64VpRegisters {
    fn default() -> Self {
        Self {
            x: [0; 29], x_fp: 0, x_lr: 0,
            elr_el2: 0, spsr_el2: 0, esr_el1: 0, spsr_el1: 0,
            far_el1: 0, par_el1: 0, elr_el1: 0,
            sp_el0: 0, sp_el1: 0,
            afsr0_el1: 0, afsr1_el1: 0,
            vmpidr_el2: 0, vpidr_el2: 0,
            sctlr_el1: 0, actlr_el1: 0, tcr_el1: 0, mair_el1: 0,
            tpidr_el1: 0, amair_el1: 0, tpidrro_el0: 0, tpidr_el0: 0,
            contextidr_el1: 0, cpacr_el1: 0, csselr_el1: 0,
            cntk_ctl_el1: 0, cntv_cval_el0: 0, cntv_ctl_el0: 0,
            ttbr0_el1: 0, ttbr1_el1: 0, vbar_el1: 0,
            q: FromZeros::new_zeroed(), fpsr: 0, fpcr: 0,
        }
    }
}

/// Per-VTL register state for a VP.
struct VtlState {
    vtl: u8,
    regs: VpState,
}

/// Per-VP state, either x64 or ARM64.
enum VpState {
    X64(X64VpRegisters),
    Aarch64(Aarch64VpRegisters),
}

struct VpEntry {
    vp_index: u32,
    /// Register state per VTL. Single-VTL VMs have one entry (VTL 0).
    vtl_states: Vec<VtlState>,
    /// Which VTL is the active one (the one running when the dump was taken).
    active_vtl: u8,
}

/// Builds the partition state blob (chunk stream).
///
/// The blob is wrapped in a [`VidSavedStateDescriptor`] envelope and contains
/// the chunk sequence expected by `VmSavedStateDumpProvider`:
///
/// 1. Prolog (processor vendor)
/// 2. OsId (guest OS identification)
/// 3. PartitionVtl markers (one per VTL, for multi-VTL)
/// 4. VpIndices (VP present bitmap)
/// 5. Per-VP: Vp marker → VpVtlControlPage → per-VTL register chunks
/// 6. Epilog
pub struct PartitionStateBuilder {
    arch: ProcessorArch,
    os_id: u64,
    vtls: Vec<u8>,
    vps: Vec<VpEntry>,
}

impl PartitionStateBuilder {
    /// Creates a new builder for the given processor architecture.
    pub fn new(arch: ProcessorArch) -> Self {
        Self {
            arch,
            os_id: 0,
            vtls: Vec::new(),
            vps: Vec::new(),
        }
    }

    /// Sets the guest OS ID (from `HV_X64_MSR_GUEST_OS_ID`).
    ///
    /// Zero for unenlightened guests. WinDbg uses this to detect the
    /// guest OS type (Windows, Linux, etc.).
    pub fn set_os_id(&mut self, os_id: u64) {
        self.os_id = os_id;
    }

    /// Adds an x64 VP to the saved state (single-VTL, VTL 0).
    pub fn add_x64_vp(&mut self, vp_index: u32, regs: &X64VpRegisters) {
        self.vps.push(VpEntry {
            vp_index,
            vtl_states: vec![VtlState {
                vtl: 0,
                regs: VpState::X64(regs.clone()),
            }],
            active_vtl: 0,
        });
    }

    /// Adds an ARM64 VP to the saved state (single-VTL, VTL 0).
    pub fn add_aarch64_vp(&mut self, vp_index: u32, regs: &Aarch64VpRegisters) {
        self.vps.push(VpEntry {
            vp_index,
            vtl_states: vec![VtlState {
                vtl: 0,
                regs: VpState::Aarch64(regs.clone()),
            }],
            active_vtl: 0,
        });
    }

    /// Adds an x64 VP with multiple VTLs.
    ///
    /// `vtl_regs` is a list of `(vtl, registers)` pairs. `active_vtl` is
    /// the VTL that was running when the dump was taken.
    pub fn add_x64_vp_multi_vtl(
        &mut self,
        vp_index: u32,
        vtl_regs: &[(u8, X64VpRegisters)],
        active_vtl: u8,
    ) {
        let vtl_states = vtl_regs
            .iter()
            .map(|(vtl, regs)| VtlState {
                vtl: *vtl,
                regs: VpState::X64(regs.clone()),
            })
            .collect();

        // Track VTLs at partition level
        for (vtl, _) in vtl_regs {
            if !self.vtls.contains(vtl) {
                self.vtls.push(*vtl);
            }
        }
        self.vtls.sort();

        self.vps.push(VpEntry {
            vp_index,
            vtl_states,
            active_vtl,
        });
    }

    /// Adds an ARM64 VP with multiple VTLs.
    pub fn add_aarch64_vp_multi_vtl(
        &mut self,
        vp_index: u32,
        vtl_regs: &[(u8, Aarch64VpRegisters)],
        active_vtl: u8,
    ) {
        let vtl_states = vtl_regs
            .iter()
            .map(|(vtl, regs)| VtlState {
                vtl: *vtl,
                regs: VpState::Aarch64(regs.clone()),
            })
            .collect();

        for (vtl, _) in vtl_regs {
            if !self.vtls.contains(vtl) {
                self.vtls.push(*vtl);
            }
        }
        self.vtls.sort();

        self.vps.push(VpEntry {
            vp_index,
            vtl_states,
            active_vtl,
        });
    }

    /// Builds the partition state blob.
    ///
    /// Returns the complete blob including the `VidSavedStateDescriptor`
    /// envelope, ready to be stored as the
    /// `/savedstate/savedVM/partition_state` array value.
    pub fn finish(&self) -> Vec<u8> {
        let mut chunks = Vec::new();

        // Prolog
        self.write_prolog(&mut chunks);

        // OsId (partition-level)
        self.write_os_id(&mut chunks);

        // PartitionVtl markers (for multi-VTL)
        if self.vtls.len() > 1 {
            for &vtl in &self.vtls {
                self.write_partition_vtl(&mut chunks, vtl);
            }
        }

        // VpIndices
        self.write_vp_indices(&mut chunks);

        // Per-VP chunks
        for vp in &self.vps {
            self.write_vp_marker(&mut chunks, vp.vp_index);

            if vp.vtl_states.len() > 1 {
                // Multi-VTL: emit VpVtlControlPage, then per-VTL register sets
                self.write_vp_vtl_control_page(&mut chunks, vp.active_vtl);
                for vtl_state in &vp.vtl_states {
                    self.write_vp_vtl_marker(&mut chunks, vtl_state.vtl);
                    match &vtl_state.regs {
                        VpState::X64(regs) => self.write_x64_vp_chunks(&mut chunks, regs),
                        VpState::Aarch64(regs) => {
                            self.write_aarch64_vp_chunks(&mut chunks, regs)
                        }
                    }
                }
            } else {
                // Single-VTL: emit register chunks directly
                match &vp.vtl_states[0].regs {
                    VpState::X64(regs) => self.write_x64_vp_chunks(&mut chunks, regs),
                    VpState::Aarch64(regs) => self.write_aarch64_vp_chunks(&mut chunks, regs),
                }
            }
        }

        // Epilog
        self.write_epilog(&mut chunks);

        // Wrap in VidSavedStateDescriptor envelope
        self.wrap_envelope(chunks)
    }

    fn write_prolog(&self, out: &mut Vec<u8>) {
        let mut prolog = ObSaveChunkProlog::new_zeroed();
        prolog.header = chunk_header(
            VmSaveChunkId::PROLOG,
            OB_SAVE_CHUNK_PROLOG_SIZE - size_of::<VmSaveChunkHeader>(),
        );
        prolog.undefined_tag = VM_SAVE_CHUNK_TAG_UNDEFINED;
        prolog.is_summary_save_state = 0;
        prolog.vendor = match self.arch {
            ProcessorArch::X64 => HvProcessorVendor::INTEL,
            ProcessorArch::Aarch64 => HvProcessorVendor::ARM,
        };
        out.extend_from_slice(prolog.as_bytes());
    }

    fn write_os_id(&self, out: &mut Vec<u8>) {
        let mut chunk = PtSaveChunkOsId::new_zeroed();
        chunk.header = chunk_header(
            VmSaveChunkId::OS_ID,
            size_of::<PtSaveChunkOsId>() - size_of::<VmSaveChunkHeader>(),
        );
        chunk.os_id = self.os_id;
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_vp_indices(&self, out: &mut Vec<u8>) {
        let mut chunk = VpSaveChunkVpIndices::new_zeroed();

        // Data length = sizeof(bsp) + sizeof(vp_present_map) + sizeof(_padding)
        chunk.header = chunk_header(
            VmSaveChunkId::VP_INDICES,
            size_of::<VpSaveChunkVpIndices>() - size_of::<VmSaveChunkHeader>(),
        );

        // BSP is the first VP
        chunk.bsp = self.vps.first().map_or(0, |vp| vp.vp_index);

        // Set bits for present VPs
        for vp in &self.vps {
            let byte_idx = vp.vp_index as usize / 8;
            let bit_idx = vp.vp_index as usize % 8;
            if byte_idx < chunk.vp_present_map.len() {
                chunk.vp_present_map[byte_idx] |= 1 << bit_idx;
            }
        }

        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_vp_marker(&self, out: &mut Vec<u8>, vp_index: u32) {
        let mut chunk = ObSaveChunkVp::new_zeroed();
        chunk.header = chunk_header(
            VmSaveChunkId::VP,
            size_of::<ObSaveChunkVp>() - size_of::<VmSaveChunkHeader>(),
        );
        chunk.vp_index = vp_index;
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_partition_vtl(&self, out: &mut Vec<u8>, vtl: u8) {
        let mut chunk = ObSaveChunkVtl::new_zeroed();
        chunk.header = chunk_header(
            VmSaveChunkId::PARTITION_VTL,
            size_of::<ObSaveChunkVtl>() - size_of::<VmSaveChunkHeader>(),
        );
        chunk.vtl = vtl;
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_vp_vtl_marker(&self, out: &mut Vec<u8>, vtl: u8) {
        let mut chunk = ObSaveChunkVtl::new_zeroed();
        chunk.header = chunk_header(
            VmSaveChunkId::VP_VTL,
            size_of::<ObSaveChunkVtl>() - size_of::<VmSaveChunkHeader>(),
        );
        chunk.vtl = vtl;
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_vp_vtl_control_page(&self, out: &mut Vec<u8>, active_vtl: u8) {
        let mut chunk = VsmSaveChunkVpVtlControlPage::new_zeroed();
        chunk.header = chunk_header(
            VmSaveChunkId::VP_VTL_CONTROL_PAGE,
            size_of::<VsmSaveChunkVpVtlControlPage>() - size_of::<VmSaveChunkHeader>(),
        );
        // The VTL control data encodes which VTL is active. The first
        // 4 bytes (entry_reason) indicate the VTL return state. For a
        // simple dump, just mark the VTL as runnable.
        chunk.vtl_is_runnable = 1;
        // Encode active VTL in the control contents. The entry_reason
        // field (first u32 of vp_assist_page_vtl_control_contents) is
        // used by the parser to determine VTL switching state.
        // For dumps, the active VTL is inferred from which VTL has
        // runnable state, so we just set vtl_is_runnable.
        let _ = active_vtl; // Active VTL is communicated by chunk ordering
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_epilog(&self, out: &mut Vec<u8>) {
        let chunk = ObSaveChunkEpilog {
            header: chunk_header(VmSaveChunkId::EPILOG, 0),
        };
        out.extend_from_slice(chunk.as_bytes());
    }

    fn write_x64_vp_chunks(&self, out: &mut Vec<u8>, regs: &X64VpRegisters) {
        // GP Registers
        {
            let mut chunk = VpX64SaveChunkGpRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_GP_REGISTERS,
                size_of::<VpX64SaveChunkGpRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.rax = regs.rax;
            chunk.rcx = regs.rcx;
            chunk.rdx = regs.rdx;
            chunk.rbx = regs.rbx;
            chunk.rsp = regs.rsp;
            chunk.rbp = regs.rbp;
            chunk.rsi = regs.rsi;
            chunk.rdi = regs.rdi;
            chunk.r8 = regs.r8;
            chunk.r9 = regs.r9;
            chunk.r10 = regs.r10;
            chunk.r11 = regs.r11;
            chunk.r12 = regs.r12;
            chunk.r13 = regs.r13;
            chunk.r14 = regs.r14;
            chunk.r15 = regs.r15;
            chunk.rip = regs.rip;
            chunk.rflags = regs.rflags;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Control Registers
        {
            let mut chunk = SynicX64SaveChunkControlRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_VTL_CONTROL_REGISTERS,
                size_of::<SynicX64SaveChunkControlRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.cr0 = regs.cr0;
            chunk.cr2 = regs.cr2;
            chunk.cr3 = regs.cr3;
            chunk.cr4 = regs.cr4;
            chunk.cr8 = regs.cr8;
            chunk.efer = regs.efer;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Segment Registers
        {
            let mut chunk = VpX64SaveChunkSegmentRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_SEGMENT_REGISTERS,
                size_of::<VpX64SaveChunkSegmentRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.es = regs.es;
            chunk.cs = regs.cs;
            chunk.ss = regs.ss;
            chunk.ds = regs.ds;
            chunk.fs = regs.fs;
            chunk.gs = regs.gs;
            chunk.ldtr = regs.ldtr;
            chunk.tr = regs.tr;
            chunk.cpl = regs.cpl;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Table Registers
        {
            let mut chunk = VpX64SaveChunkTableRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_TABLE_REGISTERS,
                size_of::<VpX64SaveChunkTableRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.idtr = regs.idtr;
            chunk.gdtr = regs.gdtr;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Debug Registers
        {
            let mut chunk = VpX64SaveChunkDebugRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_DEBUG_REGISTERS,
                size_of::<VpX64SaveChunkDebugRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.dr0 = regs.dr0;
            chunk.dr1 = regs.dr1;
            chunk.dr2 = regs.dr2;
            chunk.dr3 = regs.dr3;
            chunk.dr6 = regs.dr6;
            chunk.dr7 = regs.dr7;
            out.extend_from_slice(chunk.as_bytes());
        }

        // FP Registers
        {
            let mut chunk = VpX64SaveChunkFpRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_FP_REGISTERS,
                size_of::<VpX64SaveChunkFpRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.xmm = regs.xmm;
            chunk.fp_mmx = regs.fp_mmx;
            chunk.fp_control_status = regs.fp_control_status;
            chunk.xmm_control_status = regs.xmm_control_status;
            out.extend_from_slice(chunk.as_bytes());
        }
    }

    fn write_aarch64_vp_chunks(&self, out: &mut Vec<u8>, regs: &Aarch64VpRegisters) {
        // GP Registers
        {
            let mut chunk = VpArm64SaveChunkGpRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_GP_REGISTERS,
                size_of::<VpArm64SaveChunkGpRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.x = regs.x;
            chunk.x_fp = regs.x_fp;
            chunk.x_lr = regs.x_lr;
            chunk.elr_el2 = regs.elr_el2;
            chunk.spsr_el2 = regs.spsr_el2;
            chunk.esr_el1 = regs.esr_el1;
            chunk.spsr_el1 = regs.spsr_el1;
            chunk.far_el1 = regs.far_el1;
            chunk.par_el1 = regs.par_el1;
            chunk.elr_el1 = regs.elr_el1;
            chunk.sp_el0 = regs.sp_el0;
            chunk.sp_el1 = regs.sp_el1;
            chunk.afsr0_el1 = regs.afsr0_el1;
            chunk.afsr1_el1 = regs.afsr1_el1;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Control Registers
        {
            let mut chunk = SynicArm64SaveChunkControlRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_VTL_CONTROL_REGISTERS,
                size_of::<SynicArm64SaveChunkControlRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.vmpidr_el2 = regs.vmpidr_el2;
            chunk.vpidr_el2 = regs.vpidr_el2;
            chunk.sctlr_el1 = regs.sctlr_el1;
            chunk.actlr_el1 = regs.actlr_el1;
            chunk.tcr_el1 = regs.tcr_el1;
            chunk.mair_el1 = regs.mair_el1;
            chunk.tpidr_el1 = regs.tpidr_el1;
            chunk.amair_el1 = regs.amair_el1;
            chunk.tpidrro_el0 = regs.tpidrro_el0;
            chunk.tpidr_el0 = regs.tpidr_el0;
            chunk.contextidr_el1 = regs.contextidr_el1;
            chunk.cpacr_el1 = regs.cpacr_el1;
            chunk.csselr_el1 = regs.csselr_el1;
            chunk.cntk_ctl_el1 = regs.cntk_ctl_el1;
            chunk.cntv_cval_el0 = regs.cntv_cval_el0;
            chunk.cntv_ctl_el0 = regs.cntv_ctl_el0;
            out.extend_from_slice(chunk.as_bytes());
        }

        // Table Registers
        {
            let mut chunk = VpArm64SaveChunkTableRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_TABLE_REGISTERS,
                size_of::<VpArm64SaveChunkTableRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.ttbr0_el1 = regs.ttbr0_el1;
            chunk.ttbr1_el1 = regs.ttbr1_el1;
            chunk.vbar_el1 = regs.vbar_el1;
            out.extend_from_slice(chunk.as_bytes());
        }

        // FP Registers
        {
            let mut chunk = VpArm64SaveChunkFpRegisters::new_zeroed();
            chunk.header = chunk_header(
                VmSaveChunkId::VP_FP_REGISTERS,
                size_of::<VpArm64SaveChunkFpRegisters>() - size_of::<VmSaveChunkHeader>(),
            );
            chunk.q = regs.q;
            chunk.fpsr = regs.fpsr;
            chunk.fpcr = regs.fpcr;
            out.extend_from_slice(chunk.as_bytes());
        }
    }

    /// Wraps the chunk stream in a VidSavedStateDescriptor envelope.
    ///
    /// Layout:
    /// - VidSavedStateDescriptor (24 bytes)
    /// - 16 bytes of alignment padding (skipped by parser)
    /// - Chunk data
    fn wrap_envelope(&self, chunks: Vec<u8>) -> Vec<u8> {
        let descriptor_size = size_of::<VidSavedStateDescriptor>() as u64;
        let header_size = descriptor_size;
        // Parser starts reading at header_size + 16
        let total_size = header_size + 16 + chunks.len() as u64;

        let descriptor = VidSavedStateDescriptor {
            descriptor_size,
            header_size,
            total_size,
        };

        let mut blob = Vec::with_capacity(total_size as usize);
        blob.extend_from_slice(descriptor.as_bytes());
        // 16 bytes alignment padding
        blob.extend_from_slice(&[0u8; 16]);
        blob.extend_from_slice(&chunks);
        blob
    }
}

/// Creates a chunk header with the given ID and data length.
fn chunk_header(id: VmSaveChunkId, data_length: usize) -> VmSaveChunkHeader {
    VmSaveChunkHeader {
        id,
        data_length: data_length as u32,
        _padding: [0; 8],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::FromBytes;

    #[test]
    fn build_minimal_x64_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.set_os_id(0);

        let mut regs = X64VpRegisters::default();
        regs.rip = 0xFFFFF800_12345678;
        regs.rsp = 0xFFFFF800_AABBCCDD;
        regs.cr3 = 0x1AD000;
        regs.cs = HvX64SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        builder.add_x64_vp(0, &regs);

        let blob = builder.finish();

        // Verify the descriptor
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;
        assert_eq!(desc.descriptor_size, 24);
        assert_eq!(desc.total_size, blob.len() as u64);

        // Verify we can find the prolog at offset header_size + 16
        let chunk_start = desc.header_size as usize + 16;
        let prolog_header =
            VmSaveChunkHeader::read_from_prefix(&blob[chunk_start..])
                .unwrap()
                .0;
        assert_eq!(prolog_header.id, VmSaveChunkId::PROLOG);
        assert_eq!(prolog_header.data_length, 4064);
    }

    #[test]
    fn build_minimal_aarch64_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::Aarch64);

        let mut regs = Aarch64VpRegisters::default();
        regs.x[0] = 0x1234;
        regs.sctlr_el1 = 0x30D00800;
        regs.ttbr0_el1 = 0x40000;
        builder.add_aarch64_vp(0, &regs);

        let blob = builder.finish();

        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;
        assert_eq!(desc.total_size, blob.len() as u64);

        // Verify prolog has ARM vendor
        let chunk_start = desc.header_size as usize + 16;
        let prolog =
            ObSaveChunkProlog::read_from_prefix(&blob[chunk_start..])
                .unwrap()
                .0;
        assert_eq!(prolog.vendor, HvProcessorVendor::ARM);
    }

    #[test]
    fn multi_vp_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);

        for i in 0..4u32 {
            let mut regs = X64VpRegisters::default();
            regs.rip = 0x1000 + i as u64;
            builder.add_x64_vp(i, &regs);
        }

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        // Walk through chunks to verify structure
        let mut offset = desc.header_size as usize + 16;
        let mut chunk_ids = Vec::new();
        while offset + size_of::<VmSaveChunkHeader>() <= blob.len() {
            let header =
                VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                    .unwrap()
                    .0;
            chunk_ids.push(header.id);
            if header.id == VmSaveChunkId::EPILOG {
                break;
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }

        // Expected: Prolog, OsId, VpIndices, then 4x (Vp, GP, Control, Seg, Table, Debug, FP), Epilog
        assert_eq!(chunk_ids[0], VmSaveChunkId::PROLOG);
        assert_eq!(chunk_ids[1], VmSaveChunkId::OS_ID);
        assert_eq!(chunk_ids[2], VmSaveChunkId::VP_INDICES);

        // Each VP has 7 chunks: Vp + GP + Control + Seg + Table + Debug + FP
        let per_vp_count = 7;
        for vp_idx in 0..4u32 {
            let base = 3 + vp_idx as usize * per_vp_count;
            assert_eq!(chunk_ids[base], VmSaveChunkId::VP, "VP marker for VP {vp_idx}");
            assert_eq!(chunk_ids[base + 1], VmSaveChunkId::VP_GP_REGISTERS);
            assert_eq!(chunk_ids[base + 2], VmSaveChunkId::VP_VTL_CONTROL_REGISTERS);
            assert_eq!(chunk_ids[base + 3], VmSaveChunkId::VP_SEGMENT_REGISTERS);
            assert_eq!(chunk_ids[base + 4], VmSaveChunkId::VP_TABLE_REGISTERS);
            assert_eq!(chunk_ids[base + 5], VmSaveChunkId::VP_DEBUG_REGISTERS);
            assert_eq!(chunk_ids[base + 6], VmSaveChunkId::VP_FP_REGISTERS);
        }

        let last = chunk_ids.last().unwrap();
        assert_eq!(*last, VmSaveChunkId::EPILOG);
    }

    #[test]
    fn vp_indices_bitmap() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        // Add VPs 0, 2, 7
        for &i in &[0u32, 2, 7] {
            builder.add_x64_vp(i, &X64VpRegisters::default());
        }

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        // Find VpIndices chunk
        let mut offset = desc.header_size as usize + 16;
        loop {
            let header =
                VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                    .unwrap()
                    .0;
            if header.id == VmSaveChunkId::VP_INDICES {
                let chunk =
                    VpSaveChunkVpIndices::read_from_prefix(&blob[offset..])
                        .unwrap()
                        .0;
                // BSP should be 0 (first VP added)
                assert_eq!(chunk.bsp, 0);
                // Bits 0, 2, 7 should be set
                assert_eq!(chunk.vp_present_map[0], 0b10000101); // bits 0, 2, 7
                break;
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }
    }

    #[test]
    fn x64_register_values_roundtrip() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);

        let mut regs = X64VpRegisters::default();
        regs.rax = 0xAAAA;
        regs.rcx = 0xBBBB;
        regs.rip = 0xFFFFF800_12345678;
        regs.cr3 = 0x1AD000;
        regs.cr0 = 0x80050033;
        regs.efer = 0xD01;
        regs.cs = HvX64SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        regs.idtr = HvX64TableRegister {
            pad: [0; 3],
            limit: 0xFFF,
            base: 0xFFFFF800_00000000,
        };
        builder.add_x64_vp(0, &regs);

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        // Walk chunks and verify GP register values
        let mut offset = desc.header_size as usize + 16;
        loop {
            let header =
                VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                    .unwrap()
                    .0;
            if header.id == VmSaveChunkId::VP_GP_REGISTERS {
                let chunk =
                    VpX64SaveChunkGpRegisters::read_from_prefix(&blob[offset..])
                        .unwrap()
                        .0;
                assert_eq!(chunk.rax, 0xAAAA);
                assert_eq!(chunk.rcx, 0xBBBB);
                assert_eq!(chunk.rip, 0xFFFFF800_12345678);
                break;
            }
            if header.id == VmSaveChunkId::EPILOG {
                panic!("GP registers chunk not found");
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }
    }

    #[test]
    fn multi_vtl_chunk_structure() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);

        let mut vtl0_regs = X64VpRegisters::default();
        vtl0_regs.rip = 0x1000;
        vtl0_regs.cr3 = 0x1AD000;

        let mut vtl1_regs = X64VpRegisters::default();
        vtl1_regs.rip = 0x2000;
        vtl1_regs.cr3 = 0x2AD000;

        builder.add_x64_vp_multi_vtl(
            0,
            &[(0, vtl0_regs), (1, vtl1_regs)],
            0, // active VTL
        );

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        // Walk chunks
        let mut offset = desc.header_size as usize + 16;
        let mut chunk_ids = Vec::new();
        while offset + size_of::<VmSaveChunkHeader>() <= blob.len() {
            let header =
                VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                    .unwrap()
                    .0;
            chunk_ids.push(header.id);
            if header.id == VmSaveChunkId::EPILOG {
                break;
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }

        // Verify: Prolog, OsId, PartitionVtl(0), PartitionVtl(1), VpIndices,
        //         Vp, VpVtlControlPage, VpVtl(0), <regs>, VpVtl(1), <regs>, Epilog
        assert_eq!(chunk_ids[0], VmSaveChunkId::PROLOG);
        assert_eq!(chunk_ids[1], VmSaveChunkId::OS_ID);
        assert_eq!(chunk_ids[2], VmSaveChunkId::PARTITION_VTL);
        assert_eq!(chunk_ids[3], VmSaveChunkId::PARTITION_VTL);
        assert_eq!(chunk_ids[4], VmSaveChunkId::VP_INDICES);
        assert_eq!(chunk_ids[5], VmSaveChunkId::VP);
        assert_eq!(chunk_ids[6], VmSaveChunkId::VP_VTL_CONTROL_PAGE);
        assert_eq!(chunk_ids[7], VmSaveChunkId::VP_VTL);
        // VTL 0 register chunks (6 chunks: GP, Control, Seg, Table, Debug, FP)
        assert_eq!(chunk_ids[8], VmSaveChunkId::VP_GP_REGISTERS);
        assert_eq!(chunk_ids[14], VmSaveChunkId::VP_VTL); // VTL 1
        assert_eq!(chunk_ids[15], VmSaveChunkId::VP_GP_REGISTERS); // VTL 1 regs
        let last = chunk_ids.last().unwrap();
        assert_eq!(*last, VmSaveChunkId::EPILOG);
    }
}
