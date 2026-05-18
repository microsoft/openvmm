// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor save/restore chunk definitions.
//!
//! These are the canonical structures from `onecore/hv/hvx/inc/vm.h`, used
//! for serializing VM processor state into the partition state blob stored
//! in `.vmrs` files.

use crate::AlignedU128;
use crate::HvX64SegmentRegister;
use crate::HvX64TableRegister;
use core::mem::size_of;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// ============================================================
// Chunk IDs
// ============================================================

open_enum! {
    /// Save/restore chunk IDs from `VM_SAVE_CHUNK_ID`.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum VmSaveChunkId: u32 {

        // Framing
        PROLOG                      = 0x0000_0000,
        EPILOG                      = 0xF000_0000,

        // Partition-level (0x2xxxxxxx)
        PROCESSOR_CPUID_DATA        = 0x2000_0000,
        OS_ID                       = 0x2000_1000,
        PARTITION_VTL               = 0x2000_8000,
        PARTITION_VSM_CONFIG        = 0x2000_A000,

        // Per-VP (0x3xxxxxxx)
        VP_INDICES                  = 0x3000_0000,
        VP                          = 0x3000_1000,
        VP_CORE                     = 0x3000_3000,
        VP_GP_REGISTERS             = 0x3000_5000,
        VP_FP_REGISTERS             = 0x3000_6000,
        VP_VTL_CONTROL_REGISTERS    = 0x3000_7000,
        VP_DEBUG_REGISTERS          = 0x3000_8000,
        VP_SEGMENT_REGISTERS        = 0x3000_9000,
        VP_TABLE_REGISTERS          = 0x3000_A000,
        VP_VIRTUAL_MSRS             = 0x3000_B000,
        VP_XSAVE_CONTROL_REGISTERS  = 0x3000_D000,
        VP_XSAVE_AVX_REGISTERS      = 0x3000_E000,
        VP_SYNIC_APIC_STATE         = 0x3000_F000,
        VP_SYNIC_MSRS               = 0x3001_0000,
        VP_VTL_CONTROL_PAGE         = 0x3001_8000,
        VP_VTL                      = 0x3001_A000,
        VP_NESTED_BASE              = 0x3002_1000,
        VP_NESTED_CURRENT_VMCS      = 0x3002_3000,
    }
}

// ============================================================
// Chunk Header
// ============================================================

/// Header prefixed to every chunk in the partition state blob.
///
/// The C definition uses `DECLSPEC_ALIGN(16)`, making `sizeof` = 16.
/// On disk, each chunk occupies `size_of::<VmSaveChunkHeader>() + data_length`
/// bytes — there is no alignment padding between chunks in the stream.
#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VmSaveChunkHeader {
    pub id: VmSaveChunkId,
    pub data_length: u32,
    pub _padding: [u8; 8],
}

static_assertions::const_assert_eq!(size_of::<VmSaveChunkHeader>(), 16);

// ============================================================
// VID Saved State Descriptor (blob envelope)
// ============================================================

/// Envelope wrapping the partition state chunk stream.
///
/// The chunk data starts at offset `header_size + 16` from the start of
/// the blob. The first 16 bytes after `header_size` are skipped for
/// alignment.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VidSavedStateDescriptor {
    /// Size of the descriptor + any pre-data area.
    pub descriptor_size: u64,
    /// Size of the pre-data sections (descriptor + header areas).
    pub header_size: u64,
    /// Total blob size.
    pub total_size: u64,
}

// ============================================================
// Processor Vendor
// ============================================================

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvProcessorVendor: u32 {
        AMD     = 0x0000,
        INTEL   = 0x0001,
        HYGON   = 0x0002,
        ARM     = 0x0010,
    }
}

// ============================================================
// Prolog / Epilog
// ============================================================

/// Undefined tag value written to the prolog.
pub const VM_SAVE_CHUNK_TAG_UNDEFINED: u32 = 0x5054_6475; // 'duTP'

/// Prolog chunk — always 4080 bytes total.
///
/// `Header.DataLength` = 4080 - 16 = 4064.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObSaveChunkProlog {
    pub header: VmSaveChunkHeader,
    pub undefined_tag: u32,
    pub is_summary_save_state: u8,
    pub _padding: [u8; 3],
    pub vendor: HvProcessorVendor,
    pub _reserved: [u8; 4052],
}

pub const OB_SAVE_CHUNK_PROLOG_SIZE: usize = 4080;
static_assertions::const_assert_eq!(size_of::<ObSaveChunkProlog>(), OB_SAVE_CHUNK_PROLOG_SIZE);

/// Epilog chunk — header only, no data.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObSaveChunkEpilog {
    pub header: VmSaveChunkHeader,
}

// ============================================================
// VP Indices
// ============================================================

/// VP indices chunk — variable-length VP present bitmap.
///
/// The bitmap size is `header.data_length - 4` bytes. Bit N set = VP N
/// is present. Maximum of 240 VPs (30 bytes) in the canonical definition.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpSaveChunkVpIndices {
    pub header: VmSaveChunkHeader,
    pub bsp: u32,
    pub vp_present_map: [u8; 30],
    pub _padding: [u8; 14],
}

// ============================================================
// VP / VTL markers
// ============================================================

/// Per-VP marker chunk.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObSaveChunkVp {
    pub header: VmSaveChunkHeader,
    pub vp_index: u32,
    pub _padding: [u8; 12],
}

/// Per-VTL marker chunk within a VP.
///
/// Used for both `VmSaveChunkIdVpVtl` and `VmSaveChunkIdPartitionVtl`.
/// `HV_VTL` is `UINT8`.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObSaveChunkVtl {
    pub header: VmSaveChunkHeader,
    pub vtl: u8,
    pub _padding: [u8; 15],
}

/// Partition-level VTL marker (same layout as per-VP VTL marker).
pub type ObSaveChunkPartitionVtl = ObSaveChunkVtl;

// ============================================================
// OsId
// ============================================================

/// OsId chunk — contains the guest OS identification.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PtSaveChunkOsId {
    pub header: VmSaveChunkHeader,
    pub os_id: u64,
    pub _padding: [u8; 8],
}

// ============================================================
// x64 Register Chunks
// ============================================================

/// General purpose registers (x64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpX64SaveChunkGpRegisters {
    pub header: VmSaveChunkHeader,
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
}

/// Control registers (x64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SynicX64SaveChunkControlRegisters {
    pub header: VmSaveChunkHeader,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
}

/// Debug registers (x64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpX64SaveChunkDebugRegisters {
    pub header: VmSaveChunkHeader,
    pub dr0: u64,
    pub dr1: u64,
    pub dr2: u64,
    pub dr3: u64,
    pub dr6: u64,
    pub dr7: u64,
}

/// Segment registers (x64).
///
/// Uses the existing `HvX64SegmentRegister` type.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpX64SaveChunkSegmentRegisters {
    pub header: VmSaveChunkHeader,
    pub es: HvX64SegmentRegister,
    pub cs: HvX64SegmentRegister,
    pub ss: HvX64SegmentRegister,
    pub ds: HvX64SegmentRegister,
    pub fs: HvX64SegmentRegister,
    pub gs: HvX64SegmentRegister,
    pub ldtr: HvX64SegmentRegister,
    pub tr: HvX64SegmentRegister,
    pub cpl: u8,
    pub _padding: [u8; 15],
}

/// Table registers (x64) — IDTR and GDTR.
///
/// Uses the existing `HvX64TableRegister` type.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpX64SaveChunkTableRegisters {
    pub header: VmSaveChunkHeader,
    pub idtr: HvX64TableRegister,
    pub gdtr: HvX64TableRegister,
}

/// Floating-point / SSE / MMX registers (x64).
///
/// Uses `AlignedU128` for XMM and FP registers. The 16-byte aligned
/// `VmSaveChunkHeader` provides natural alignment for the HV_UINT128 fields.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpX64SaveChunkFpRegisters {
    pub header: VmSaveChunkHeader,
    pub xmm: [AlignedU128; 16],
    pub fp_mmx: [AlignedU128; 8],
    pub fp_control_status: AlignedU128,
    pub xmm_control_status: AlignedU128,
}

// ============================================================
// ARM64 Register Chunks
// ============================================================

/// General purpose registers (ARM64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpArm64SaveChunkGpRegisters {
    pub header: VmSaveChunkHeader,
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
}

/// Control registers (ARM64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SynicArm64SaveChunkControlRegisters {
    pub header: VmSaveChunkHeader,
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
}

/// Table registers (ARM64) — TTBR0, TTBR1, VBAR.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpArm64SaveChunkTableRegisters {
    pub header: VmSaveChunkHeader,
    pub ttbr0_el1: u64,
    pub ttbr1_el1: u64,
    pub vbar_el1: u64,
    pub _padding: [u8; 8],
}

/// Floating-point / SIMD registers (ARM64).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpArm64SaveChunkFpRegisters {
    pub header: VmSaveChunkHeader,
    pub q: [AlignedU128; 32],
    pub fpsr: u64,
    pub fpcr: u64,
}

// ============================================================
// VP VTL Control Page
// ============================================================

/// Size of VTL control data in the VP assist page.
pub const VSM_SAVE_VP_VTL_CONTROL_BYTES: usize = 24;

/// VP VTL control page chunk.
///
/// Contains the VTL control contents from the VP assist page and
/// whether the VTL is runnable.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VsmSaveChunkVpVtlControlPage {
    pub header: VmSaveChunkHeader,
    pub vp_assist_page_vtl_control_contents: [u8; VSM_SAVE_VP_VTL_CONTROL_BYTES],
    pub vtl_is_runnable: u8,
    pub _padding: [u8; 7],
}


