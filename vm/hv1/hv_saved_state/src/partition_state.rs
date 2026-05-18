// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition state blob builder.
//!
//! Constructs the chunk stream that goes into the
//! `/savedstate/savedVM/partition_state` key in a `.vmrs` file. The format
//! matches the hypervisor's save/restore chunk stream as parsed by
//! `VmSavedStateDumpProvider.dll`.
//!
//! Accepts VP register state using the `virt` crate types directly, so
//! integration with the rest of OpenVMM is straightforward.

use hvdef::AlignedU128;
use hvdef::save_restore::*;
use std::mem::size_of;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Shorthand for the chunk header size, used in data_length calculations.
const HEADER: usize = size_of::<VmSaveChunkHeader>();

/// Processor architecture for the saved state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcessorArch {
    /// x86-64 / AMD64
    X64,
    /// ARM64 / AArch64
    Aarch64,
}

/// x64 VP state for dump generation.
///
/// Groups the virt-layer register types needed to populate the save/restore
/// chunks. All fields are optional except `registers`.
pub struct X64VpState {
    /// General-purpose, segment, table, and control registers.
    pub registers: virt::x86::vp::Registers,
    /// Debug registers (DR0–DR7). Zeroed if `None`.
    pub debug_registers: Option<virt::x86::vp::DebugRegisters>,
    /// XSAVE state (FP/SSE/AVX). Zeroed if `None`.
    pub xsave: Option<virt::x86::vp::Xsave>,
}

/// ARM64 VP state for dump generation.
pub struct Aarch64VpState {
    /// General-purpose registers, SP, PC, CPSR.
    pub registers: virt::aarch64::vp::Registers,
    /// System registers (SCTLR, TTBR, TCR, etc.). Zeroed if `None`.
    pub system_registers: Option<virt::aarch64::vp::SystemRegisters>,
}

/// Per-VTL register state for a VP.
enum VpState {
    X64(X64VpState),
    Aarch64(Aarch64VpState),
}

struct VtlState {
    vtl: u8,
    regs: VpState,
}

struct VpEntry {
    vp_index: u32,
    vtl_states: Vec<VtlState>,
    /// Which VTL was active when the dump was taken. Currently unused
    /// (the parser infers it from vtl_is_runnable), but stored for
    /// future use.
    #[allow(dead_code)]
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
/// 5. Per-VP: Vp marker → per-VTL register chunks
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
    pub fn add_x64_vp(&mut self, vp_index: u32, state: X64VpState) {
        self.vps.push(VpEntry {
            vp_index,
            vtl_states: vec![VtlState {
                vtl: 0,
                regs: VpState::X64(state),
            }],
            active_vtl: 0,
        });
    }

    /// Adds an ARM64 VP to the saved state (single-VTL, VTL 0).
    pub fn add_aarch64_vp(&mut self, vp_index: u32, state: Aarch64VpState) {
        self.vps.push(VpEntry {
            vp_index,
            vtl_states: vec![VtlState {
                vtl: 0,
                regs: VpState::Aarch64(state),
            }],
            active_vtl: 0,
        });
    }

    /// Adds an x64 VP with multiple VTLs.
    ///
    /// `vtl_states` is a list of `(vtl, state)` pairs. `active_vtl` is
    /// the VTL that was running when the dump was taken.
    pub fn add_x64_vp_multi_vtl(
        &mut self,
        vp_index: u32,
        vtl_states: Vec<(u8, X64VpState)>,
        active_vtl: u8,
    ) {
        for &(vtl, _) in &vtl_states {
            if !self.vtls.contains(&vtl) {
                self.vtls.push(vtl);
            }
        }
        self.vtls.sort();

        let states = vtl_states
            .into_iter()
            .map(|(vtl, state)| VtlState {
                vtl,
                regs: VpState::X64(state),
            })
            .collect();

        self.vps.push(VpEntry {
            vp_index,
            vtl_states: states,
            active_vtl,
        });
    }

    /// Adds an ARM64 VP with multiple VTLs.
    pub fn add_aarch64_vp_multi_vtl(
        &mut self,
        vp_index: u32,
        vtl_states: Vec<(u8, Aarch64VpState)>,
        active_vtl: u8,
    ) {
        for &(vtl, _) in &vtl_states {
            if !self.vtls.contains(&vtl) {
                self.vtls.push(vtl);
            }
        }
        self.vtls.sort();

        let states = vtl_states
            .into_iter()
            .map(|(vtl, state)| VtlState {
                vtl,
                regs: VpState::Aarch64(state),
            })
            .collect();

        self.vps.push(VpEntry {
            vp_index,
            vtl_states: states,
            active_vtl,
        });
    }

    /// Builds the partition state blob.
    ///
    /// Returns the complete blob including the `VidSavedStateDescriptor`
    /// envelope, ready to be stored as the
    /// `/savedstate/savedVM/partition_state` array value.
    pub fn finish(&self) -> Vec<u8> {
        let descriptor_size = size_of::<VidSavedStateDescriptor>() as u64;
        let envelope_prefix = size_of::<VidSavedStateDescriptor>() + 16;

        // Reserve space for the descriptor + alignment padding, filled in at the end.
        let mut blob = vec![0u8; envelope_prefix];

        self.write_prolog(&mut blob);
        self.write_os_id(&mut blob);

        if self.vtls.len() > 1 {
            for &vtl in &self.vtls {
                self.write_partition_vtl(&mut blob, vtl);
            }
        }

        self.write_vp_indices(&mut blob);

        for vp in &self.vps {
            self.write_vp_marker(&mut blob, vp.vp_index);

            if vp.vtl_states.len() > 1 {
                self.write_vp_vtl_control_page(&mut blob);
                for vtl_state in &vp.vtl_states {
                    self.write_vp_vtl_marker(&mut blob, vtl_state.vtl);
                    self.write_vp_chunks(&mut blob, &vtl_state.regs);
                }
            } else {
                self.write_vp_chunks(&mut blob, &vp.vtl_states[0].regs);
            }
        }

        self.write_epilog(&mut blob);

        // Patch the descriptor now that total size is known.
        let descriptor = VidSavedStateDescriptor {
            descriptor_size,
            header_size: descriptor_size,
            total_size: blob.len() as u64,
        };
        blob[..size_of::<VidSavedStateDescriptor>()].copy_from_slice(descriptor.as_bytes());

        blob
    }

    fn write_vp_chunks(&self, out: &mut Vec<u8>, state: &VpState) {
        match state {
            VpState::X64(s) => self.write_x64_vp_chunks(out, s),
            VpState::Aarch64(s) => self.write_aarch64_vp_chunks(out, s),
        }
    }

    // ---- Framing chunks ----

    fn write_prolog(&self, out: &mut Vec<u8>) {
        // Prolog is 4080 bytes with a large reserved region — new_zeroed
        // is appropriate here since the struct is mostly padding.
        let mut prolog = ObSaveChunkProlog::new_zeroed();
        prolog.header = chunk_header_for::<ObSaveChunkProlog>(VmSaveChunkId::PROLOG);
        prolog.undefined_tag = VM_SAVE_CHUNK_TAG_UNDEFINED;
        prolog.vendor = match self.arch {
            ProcessorArch::X64 => HvProcessorVendor::INTEL,
            ProcessorArch::Aarch64 => HvProcessorVendor::ARM,
        };
        out.extend_from_slice(prolog.as_bytes());
    }

    fn write_os_id(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(
            PtSaveChunkOsId {
                header: chunk_header_for::<PtSaveChunkOsId>(VmSaveChunkId::OS_ID),
                os_id: self.os_id,
                _padding: [0; 8],
            }
            .as_bytes(),
        );
    }

    fn write_vp_indices(&self, out: &mut Vec<u8>) {
        let mut vp_present_map = [0u8; 30];
        for vp in &self.vps {
            let byte_idx = vp.vp_index as usize / 8;
            let bit_idx = vp.vp_index as usize % 8;
            if byte_idx < vp_present_map.len() {
                vp_present_map[byte_idx] |= 1 << bit_idx;
            }
        }
        out.extend_from_slice(
            VpSaveChunkVpIndices {
                header: chunk_header_for::<VpSaveChunkVpIndices>(VmSaveChunkId::VP_INDICES),
                bsp: self.vps.first().map_or(0, |vp| vp.vp_index),
                vp_present_map,
                _padding: [0; 14],
            }
            .as_bytes(),
        );
    }

    fn write_vp_marker(&self, out: &mut Vec<u8>, vp_index: u32) {
        out.extend_from_slice(
            ObSaveChunkVp {
                header: chunk_header_for::<ObSaveChunkVp>(VmSaveChunkId::VP),
                vp_index,
                _padding: [0; 12],
            }
            .as_bytes(),
        );
    }

    fn write_partition_vtl(&self, out: &mut Vec<u8>, vtl: u8) {
        out.extend_from_slice(
            ObSaveChunkVtl {
                header: chunk_header_for::<ObSaveChunkVtl>(VmSaveChunkId::PARTITION_VTL),
                vtl,
                _padding: [0; 15],
            }
            .as_bytes(),
        );
    }

    fn write_vp_vtl_marker(&self, out: &mut Vec<u8>, vtl: u8) {
        out.extend_from_slice(
            ObSaveChunkVtl {
                header: chunk_header_for::<ObSaveChunkVtl>(VmSaveChunkId::VP_VTL),
                vtl,
                _padding: [0; 15],
            }
            .as_bytes(),
        );
    }

    fn write_vp_vtl_control_page(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(
            VsmSaveChunkVpVtlControlPage {
                header: chunk_header_for::<VsmSaveChunkVpVtlControlPage>(VmSaveChunkId::VP_VTL_CONTROL_PAGE),
                vp_assist_page_vtl_control_contents: [0; VSM_SAVE_VP_VTL_CONTROL_BYTES],
                vtl_is_runnable: 1,
                _padding: [0; 7],
            }
            .as_bytes(),
        );
    }

    fn write_epilog(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(
            ObSaveChunkEpilog {
                header: chunk_header_for::<ObSaveChunkEpilog>(VmSaveChunkId::EPILOG),
            }
            .as_bytes(),
        );
    }

    // ---- x64 register chunks ----

    fn write_x64_vp_chunks(&self, out: &mut Vec<u8>, state: &X64VpState) {
        let r = &state.registers;

        out.extend_from_slice(
            VpX64SaveChunkGpRegisters {
                header: chunk_header_for::<VpX64SaveChunkGpRegisters>(VmSaveChunkId::VP_GP_REGISTERS),
                rax: r.rax,
                rcx: r.rcx,
                rdx: r.rdx,
                rbx: r.rbx,
                rsp: r.rsp,
                rbp: r.rbp,
                rsi: r.rsi,
                rdi: r.rdi,
                r8: r.r8,
                r9: r.r9,
                r10: r.r10,
                r11: r.r11,
                r12: r.r12,
                r13: r.r13,
                r14: r.r14,
                r15: r.r15,
                rip: r.rip,
                rflags: r.rflags,
            }
            .as_bytes(),
        );

        out.extend_from_slice(
            SynicX64SaveChunkControlRegisters {
                header: chunk_header_for::<SynicX64SaveChunkControlRegisters>(VmSaveChunkId::VP_VTL_CONTROL_REGISTERS),
                cr0: r.cr0,
                cr2: r.cr2,
                cr3: r.cr3,
                cr4: r.cr4,
                cr8: r.cr8,
                efer: r.efer,
            }
            .as_bytes(),
        );

        out.extend_from_slice(
            VpX64SaveChunkSegmentRegisters {
                header: chunk_header_for::<VpX64SaveChunkSegmentRegisters>(VmSaveChunkId::VP_SEGMENT_REGISTERS),
                es: r.es.into(),
                cs: r.cs.into(),
                ss: r.ss.into(),
                ds: r.ds.into(),
                fs: r.fs.into(),
                gs: r.gs.into(),
                ldtr: r.ldtr.into(),
                tr: r.tr.into(),
                cpl: (r.cs.selector & 3) as u8,
                _padding: [0; 15],
            }
            .as_bytes(),
        );

        out.extend_from_slice(
            VpX64SaveChunkTableRegisters {
                header: chunk_header_for::<VpX64SaveChunkTableRegisters>(VmSaveChunkId::VP_TABLE_REGISTERS),
                idtr: r.idtr.into(),
                gdtr: r.gdtr.into(),
            }
            .as_bytes(),
        );

        if let Some(dr) = &state.debug_registers {
            out.extend_from_slice(
                VpX64SaveChunkDebugRegisters {
                    header: chunk_header_for::<VpX64SaveChunkDebugRegisters>(VmSaveChunkId::VP_DEBUG_REGISTERS),
                    dr0: dr.dr0,
                    dr1: dr.dr1,
                    dr2: dr.dr2,
                    dr3: dr.dr3,
                    dr6: dr.dr6,
                    dr7: dr.dr7,
                }
                .as_bytes(),
            );
        }

        if let Some(xsave) = &state.xsave {
            self.write_x64_fp_from_xsave(out, xsave);
        }
    }

    /// Extracts FP/SSE register values from XSAVE data and emits the
    /// FP registers chunk. Uses [`x86defs::xsave::Fxsave`] to parse the
    /// legacy region.
    ///
    /// The FXSAVE layout maps directly to the chunk fields:
    /// - bytes 0x00..0x10: FP control/status (FCW, FSW, FTW, FOP, FIP)
    /// - bytes 0x10..0x20: XMM control/status (FDP, MXCSR, MXCSR_MASK)
    /// - bytes 0x20..0xA0: ST0–ST7 / MM0–MM7
    /// - bytes 0xA0..0x1A0: XMM0–XMM15
    fn write_x64_fp_from_xsave(&self, out: &mut Vec<u8>, xsave: &virt::x86::vp::Xsave) {
        use x86defs::xsave::Fxsave;
        use zerocopy::FromBytes;

        let data = IntoBytes::as_bytes(xsave.data.as_slice());

        let (xmm, fp_mmx, fp_control_status, xmm_control_status) =
            if let Ok((fxsave, _)) = Fxsave::ref_from_prefix(data) {
                let fxsave_bytes = fxsave.as_bytes();
                (
                    fxsave.xmm.map(AlignedU128::from_ne_bytes),
                    fxsave.st.map(AlignedU128::from_ne_bytes),
                    AlignedU128::from_ne_bytes(fxsave_bytes[0x00..0x10].try_into().unwrap()),
                    AlignedU128::from_ne_bytes(fxsave_bytes[0x10..0x20].try_into().unwrap()),
                )
            } else {
                (
                    [AlignedU128::from(0u128); 16],
                    [AlignedU128::from(0u128); 8],
                    AlignedU128::from(0u128),
                    AlignedU128::from(0u128),
                )
            };

        out.extend_from_slice(
            VpX64SaveChunkFpRegisters {
                header: chunk_header_for::<VpX64SaveChunkFpRegisters>(VmSaveChunkId::VP_FP_REGISTERS),
                xmm,
                fp_mmx,
                fp_control_status,
                xmm_control_status,
            }
            .as_bytes(),
        );
    }

    // ---- ARM64 register chunks ----

    fn write_aarch64_vp_chunks(&self, out: &mut Vec<u8>, state: &Aarch64VpState) {
        let r = &state.registers;
        let sys = state.system_registers.unwrap_or_default();

        out.extend_from_slice(
            VpArm64SaveChunkGpRegisters {
                header: chunk_header_for::<VpArm64SaveChunkGpRegisters>(VmSaveChunkId::VP_GP_REGISTERS),
                x: [
                    r.x0, r.x1, r.x2, r.x3, r.x4, r.x5, r.x6, r.x7, r.x8, r.x9, r.x10, r.x11,
                    r.x12, r.x13, r.x14, r.x15, r.x16, r.x17, r.x18, r.x19, r.x20, r.x21,
                    r.x22, r.x23, r.x24, r.x25, r.x26, r.x27, r.x28,
                ],
                x_fp: r.fp,
                x_lr: r.lr,
                elr_el2: 0,
                spsr_el2: 0,
                esr_el1: sys.esr_el1,
                spsr_el1: 0,
                far_el1: sys.far_el1,
                par_el1: 0,
                elr_el1: sys.elr_el1,
                sp_el0: r.sp_el0,
                sp_el1: r.sp_el1,
                afsr0_el1: 0,
                afsr1_el1: 0,
            }
            .as_bytes(),
        );

        out.extend_from_slice(
            SynicArm64SaveChunkControlRegisters {
                header: chunk_header_for::<SynicArm64SaveChunkControlRegisters>(VmSaveChunkId::VP_VTL_CONTROL_REGISTERS),
                vmpidr_el2: 0,
                vpidr_el2: 0,
                sctlr_el1: sys.sctlr_el1,
                actlr_el1: 0,
                tcr_el1: sys.tcr_el1,
                mair_el1: sys.mair_el1,
                tpidr_el1: 0,
                amair_el1: 0,
                tpidrro_el0: 0,
                tpidr_el0: 0,
                contextidr_el1: 0,
                cpacr_el1: 0,
                csselr_el1: 0,
                cntk_ctl_el1: 0,
                cntv_cval_el0: 0,
                cntv_ctl_el0: 0,
            }
            .as_bytes(),
        );

        out.extend_from_slice(
            VpArm64SaveChunkTableRegisters {
                header: chunk_header_for::<VpArm64SaveChunkTableRegisters>(VmSaveChunkId::VP_TABLE_REGISTERS),
                ttbr0_el1: sys.ttbr0_el1,
                ttbr1_el1: sys.ttbr1_el1,
                vbar_el1: sys.vbar_el1,
                _padding: [0; 8],
            }
            .as_bytes(),
        );

        // FP/SIMD — not available in virt types
        out.extend_from_slice(
            VpArm64SaveChunkFpRegisters {
                header: chunk_header_for::<VpArm64SaveChunkFpRegisters>(VmSaveChunkId::VP_FP_REGISTERS),
                q: [AlignedU128::from(0u128); 32],
                fpsr: 0,
                fpcr: 0,
            }
            .as_bytes(),
        );
    }

}

/// Creates a chunk header for a chunk struct of the given total size.
///
/// `data_length` is computed as `chunk_size - sizeof(VmSaveChunkHeader)`.
fn chunk_header_for<T>(id: VmSaveChunkId) -> VmSaveChunkHeader {
    VmSaveChunkHeader {
        id,
        data_length: (size_of::<T>() - HEADER) as u32,
        _padding: [0; 8],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::FromBytes;

    fn make_x64_state(rip: u64, cr3: u64) -> X64VpState {
        let mut regs = virt::x86::vp::Registers::default();
        regs.rip = rip;
        regs.cr3 = cr3;
        regs.cr0 = 0x80050033;
        regs.efer = 0xD01;
        regs.cs = virt::x86::SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        regs.idtr = virt::x86::TableRegister {
            base: 0xFFFFF800_00000000,
            limit: 0xFFF,
        };
        X64VpState {
            registers: regs,
            debug_registers: None,
            xsave: None,
        }
    }

    #[test]
    fn build_minimal_x64_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.add_x64_vp(0, make_x64_state(0xFFFFF800_12345678, 0x1AD000));

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;
        assert_eq!(desc.descriptor_size, 24);
        assert_eq!(desc.total_size, blob.len() as u64);

        let chunk_start = desc.header_size as usize + 16;
        let prolog_header = VmSaveChunkHeader::read_from_prefix(&blob[chunk_start..])
            .unwrap()
            .0;
        assert_eq!(prolog_header.id, VmSaveChunkId::PROLOG);
    }

    #[test]
    fn build_minimal_aarch64_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::Aarch64);

        let mut regs = virt::aarch64::vp::Registers::default();
        regs.x0 = 0x1234;
        let sys = virt::aarch64::vp::SystemRegisters {
            sctlr_el1: 0x30D00800,
            ttbr0_el1: 0x40000,
            ..Default::default()
        };
        builder.add_aarch64_vp(
            0,
            Aarch64VpState {
                registers: regs,
                system_registers: Some(sys),
            },
        );

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        let chunk_start = desc.header_size as usize + 16;
        let prolog = ObSaveChunkProlog::read_from_prefix(&blob[chunk_start..])
            .unwrap()
            .0;
        assert_eq!(prolog.vendor, HvProcessorVendor::ARM);
    }

    #[test]
    fn multi_vp_blob() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        for i in 0..4u32 {
            builder.add_x64_vp(i, make_x64_state(0x1000 + i as u64, 0x1AD000));
        }

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        let mut offset = desc.header_size as usize + 16;
        let mut chunk_ids = Vec::new();
        while offset + size_of::<VmSaveChunkHeader>() <= blob.len() {
            let header = VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                .unwrap()
                .0;
            chunk_ids.push(header.id);
            if header.id == VmSaveChunkId::EPILOG {
                break;
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }

        assert_eq!(chunk_ids[0], VmSaveChunkId::PROLOG);
        assert_eq!(chunk_ids[1], VmSaveChunkId::OS_ID);
        assert_eq!(chunk_ids[2], VmSaveChunkId::VP_INDICES);
        assert_eq!(*chunk_ids.last().unwrap(), VmSaveChunkId::EPILOG);
    }

    #[test]
    fn x64_register_values_roundtrip() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        let state = make_x64_state(0xFFFFF800_12345678, 0x1AD000);
        builder.add_x64_vp(0, state);

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        let mut offset = desc.header_size as usize + 16;
        loop {
            let header = VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                .unwrap()
                .0;
            if header.id == VmSaveChunkId::VP_GP_REGISTERS {
                let chunk = VpX64SaveChunkGpRegisters::read_from_prefix(&blob[offset..])
                    .unwrap()
                    .0;
                assert_eq!(chunk.rip, 0xFFFFF800_12345678);
                return;
            }
            assert_ne!(header.id, VmSaveChunkId::EPILOG, "GP chunk not found");
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }
    }

    #[test]
    fn multi_vtl_chunk_structure() {
        let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
        builder.add_x64_vp_multi_vtl(
            0,
            vec![
                (0, make_x64_state(0x1000, 0x1AD000)),
                (1, make_x64_state(0x2000, 0x2AD000)),
            ],
            0,
        );

        let blob = builder.finish();
        let desc = VidSavedStateDescriptor::read_from_prefix(blob.as_slice())
            .unwrap()
            .0;

        let mut offset = desc.header_size as usize + 16;
        let mut chunk_ids = Vec::new();
        while offset + size_of::<VmSaveChunkHeader>() <= blob.len() {
            let header = VmSaveChunkHeader::read_from_prefix(&blob[offset..])
                .unwrap()
                .0;
            chunk_ids.push(header.id);
            if header.id == VmSaveChunkId::EPILOG {
                break;
            }
            offset += size_of::<VmSaveChunkHeader>() + header.data_length as usize;
        }

        assert_eq!(chunk_ids[0], VmSaveChunkId::PROLOG);
        assert_eq!(chunk_ids[1], VmSaveChunkId::OS_ID);
        assert_eq!(chunk_ids[2], VmSaveChunkId::PARTITION_VTL);
        assert_eq!(chunk_ids[3], VmSaveChunkId::PARTITION_VTL);
        assert_eq!(chunk_ids[4], VmSaveChunkId::VP_INDICES);
        assert_eq!(chunk_ids[5], VmSaveChunkId::VP);
        assert_eq!(chunk_ids[6], VmSaveChunkId::VP_VTL_CONTROL_PAGE);
        assert_eq!(chunk_ids[7], VmSaveChunkId::VP_VTL);
        assert_eq!(chunk_ids[8], VmSaveChunkId::VP_GP_REGISTERS);
        assert_eq!(*chunk_ids.last().unwrap(), VmSaveChunkId::EPILOG);
    }
}
