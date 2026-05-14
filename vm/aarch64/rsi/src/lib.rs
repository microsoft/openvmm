// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm CCA specific definitions, including for the Realm Service Interface (RSI).
#![allow(non_camel_case_types)]
#![expect(missing_docs)]

// TODO: CCA: A lot of the code in this module depends on who gets to package the RSI calls.
// If OpenVMM is the one that packages the RSI calls, then this module should be
// responsible for defining the RSI calls and their parameters. If the kernel driver is the one
// that packages the RSI calls, then this module should only define the data structures used
// to communicate with the kernel driver, and the RSI calls should be defined in the kernel driver.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// CCA memory permission index, used to set and get Stage 2 memory access permissions
/// via the RSI interface.
#[expect(missing_docs)]
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CcaMemPermIndex {
    Index0,
    Index1,
    Index2,
    Index3,
    Index4,
    Index5,
    Index6,
    Index7,
    Index8,
    Index9,
    Index10,
    Index11,
    Index12,
    Index13,
    #[default]
    Index14,
}

pub const RSI_PLANE_NR_GPRS: usize = 31;
pub const RSI_PLANE_GIC_NUM_LRS: usize = 16;
pub const RSI_PLANE_ENTER_FLAGS_TRAP_SIMD: u64 = 1 << 4;

/// Layout for the realm configuration page shared with the kernel driver.
#[repr(C, align(0x1000))]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cca_realm_config {
    pub ipa_width: u64,
    pub algorithm: u64,
    pub num_aux_planes: u64,
    pub gicv3_vtr: u64,
    /// 0x1000 − (4 × 8) = 0x1000 − 32 = 0xFE0
    pub pad1: [u8; 0x1000 - 4 * 8],
}

impl cca_realm_config {
    pub fn empty() -> Self {
        Self {
            ipa_width: 0,
            algorithm: 0,
            num_aux_planes: 0,
            gicv3_vtr: 0,
            pad1: [0; 0xFE0],
        }
    }
}

/// Flattened RSI plane entry buffer layout.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cca_rsi_plane_entry {
    pub flags: u64,
    pub pc: u64,
    pub pstate: u64,
    pub pad0: [u8; 0x100 - 3 * 8],
    pub gprs: [u64; RSI_PLANE_NR_GPRS],
    pub pad2: [u8; 0x100 - RSI_PLANE_NR_GPRS * 8],
    pub gicv3_hcr: u64,
    pub gicv3_lrs: [u64; RSI_PLANE_GIC_NUM_LRS],
    pub pad3: [u8; 0x100 - (1 + RSI_PLANE_GIC_NUM_LRS) * 8],
}

/// Flattened RSI plane exit buffer layout.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct cca_rsi_plane_exit {
    pub exit_reason: u64,
    pub pad1: [u8; 0x100 - 8],
    pub elr_el2: u64,
    pub esr_el2: u64,
    pub far_el2: u64,
    pub hpfar_el2: u64,
    pub pstate: u64,
    pub pad2: [u8; 0x100 - 5 * 8],
    pub gprs: [u64; RSI_PLANE_NR_GPRS],
    pub pad3: [u8; 0x100 - RSI_PLANE_NR_GPRS * 8],
    pub gicv3_hcr: u64,
    pub gicv3_lrs: [u64; RSI_PLANE_GIC_NUM_LRS],
    pub gicv3_misr: u64,
    pub gicv3_vmcr: u64,
    pub cntp_ctl_el0: u64,
    pub cntp_cval_el0: u64,
    pub cntv_ctl_el0: u64,
    pub cntv_cval_el0: u64,
    pub pad4: [u8; 0x100 - (7 + RSI_PLANE_GIC_NUM_LRS) * 8],
}

/// Combined RSI plane run page layout.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cca_rsi_plane_run {
    pub entry: cca_rsi_plane_entry,
    pub pad4: [u8; 0x800 - size_of::<cca_rsi_plane_entry>()],
    pub exit: cca_rsi_plane_exit,
    pub pad9: [u8; 0x800 - size_of::<cca_rsi_plane_exit>()],
}

const _: () = assert!(size_of::<cca_realm_config>() == 0x1000);
const _: () = assert!(size_of::<cca_rsi_plane_entry>() == 0x300);
const _: () = assert!(size_of::<cca_rsi_plane_exit>() == 0x400);
const _: () = assert!(size_of::<cca_rsi_plane_run>() == 0x1000);
