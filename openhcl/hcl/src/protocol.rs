// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structures and definitions used between the underhill kernel and HvLite.

#![expect(non_camel_case_types, missing_docs)]

use bitfield_struct::bitfield;
use hvdef::HV_MESSAGE_SIZE;
use hvdef::hypercall::HvInputVtl;
use libc::c_void;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct hcl_translate_address_info {
    pub gva_pfn: u64,
    pub gpa_pfn: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct hcl_signal_event_direct_t {
    pub vp: u32,
    pub flag: u16,
    pub sint: u8,
    pub vtl: u8,
    pub pad: u32,
    pub pad1: u16,
    pub pad2: u8,
    pub newly_signaled: u8,
}

pub const HV_VP_ASSIST_PAGE_SIGNAL_EVENT_COUNT: usize = 16;
pub const HV_VP_ASSIST_PAGE_ACTION_TYPE_SIGNAL_EVENT: u64 = 1;

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct hv_vp_assist_page_signal_event {
    pub action_type: u64,
    pub vp: u32,
    pub vtl: u8,
    pub sint: u8,
    pub flag: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct hcl_post_message_direct_t {
    pub vp: u32,
    pub sint: u32,
    pub pad: u32,
    pub pad2: u8,
    pub vtl: u8,
    pub size: u16,
    pub message: *const u8,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default)]
pub struct hcl_pfn_range_t {
    pub start_pfn: u64,
    pub last_pfn: u64,
}

#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct hcl_cpu_context_x64 {
    pub gps: [u64; 16],
    pub fx_state: x86defs::xsave::Fxsave,
    pub reserved: [u8; 384],
}

const _: () = assert!(size_of::<hcl_cpu_context_x64>() == 1024);

#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
// NOTE: x18 is managed by the hypervisor. It is assumed here be available
// for easier offset arithmetic.
pub struct hcl_cpu_context_aarch64 {
    pub x: [u64; 31],
    pub _rsvd: u64,
    pub q: [u128; 32],
    pub reserved: [u8; 256],
}

const _: () = assert!(size_of::<hcl_cpu_context_aarch64>() == 1024);

pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const CR2: usize = 4; // RSP on TdxL2EnterGuestState, CR2 on hcl_cpu_context_x64
pub const RBP: usize = 5;
pub const RSI: usize = 6;
pub const RDI: usize = 7;
pub const R8: usize = 8;
pub const R9: usize = 9;
pub const R10: usize = 10;
pub const R11: usize = 11;
pub const R12: usize = 12;
pub const R13: usize = 13;
pub const R14: usize = 14;
pub const R15: usize = 15;

pub const VTL_RETURN_ACTION_SIZE: usize = 256;

/// Kernel IPI offloading flags
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct hcl_intr_offload_flags {
    /// Enable the base level of kernel offloading support. Requires vAPIC to be enabled.
    /// HLT and Idle are accelerated by the kernel. When halted, an interrupt may be injected
    /// entirely in kernel, bypassing user-space.
    pub offload_intr_inject: bool,
    /// Handle the X2 APIC ICR register in kernel
    pub offload_x2apic: bool,
    #[bits(3)]
    reserved: u8,
    /// Halt, due to other reason. Kernel cannot clear this state.
    pub halted_other: bool,
    /// Halt, due to HLT instruction. Kernel can clear this state.
    pub halted_hlt: bool,
    /// Halt, due to guest idle. Kernel can clear this state.
    pub halted_idle: bool,
}

#[repr(C)]
pub struct hcl_run {
    pub cancel: u32,
    pub vtl_ret_action_size: u32,
    pub flags: u32,
    pub scan_proxy_irr: u8,
    pub offload_flags: hcl_intr_offload_flags,
    pub pad: [u8; 1],
    pub mode: EnterModes,
    pub exit_message: [u8; HV_MESSAGE_SIZE],
    pub context: [u8; 1024],
    pub vtl_ret_actions: [u8; VTL_RETURN_ACTION_SIZE],
    pub proxy_irr: [u32; 8],
    pub target_vtl: HvInputVtl,
    pub proxy_irr_blocked: [u32; 8],
    pub proxy_irr_exit: [u32; 8],
}

// The size of hcl_run must be less than or equal to a single 4K page.
const _: () = assert!(size_of::<hcl_run>() <= 4096);

pub const MSHV_VTL_RUN_FLAG_HALTED: u32 = 1 << 0;

#[repr(C)]
pub struct hcl_set_poll_file {
    pub cpu: i32,
    pub fd: i32,
}

#[repr(C)]
pub struct hcl_hvcall_setup {
    pub allow_bitmap_size: u64,
    pub allow_bitmap_ptr: *const u64,
}

#[repr(C)]
pub struct hcl_hvcall {
    pub control: hvdef::hypercall::Control,
    pub input_size: usize,
    pub input_data: *const c_void,
    pub status: hvdef::hypercall::HypercallOutput,
    pub output_size: usize,
    pub output_data: *const c_void,
}

pub const HCL_REG_PAGE_OFFSET: i64 = 1 << 16;
pub const HCL_VMSA_PAGE_OFFSET: i64 = 2 << 16;
pub const MSHV_APIC_PAGE_OFFSET: i64 = 3 << 16;
pub const HCL_VMSA_GUEST_VSM_PAGE_OFFSET: i64 = 4 << 16;

open_enum::open_enum! {
    /// 4 bits represent VTL0 enter mode.
    pub enum EnterMode: u8 {
        /// "Fast" mode: Enters VTL0 with scheduler ticks on, no extra cost on turning off the scheduler
        /// timers, therefore it's fast.
        FAST = 0,
        /// "Play idle" mode: Enters VTL0 with scheduler ticks off (setting the current kernel thread to
        /// idle).
        PLAY_IDLE = 1,
        /// "Idle to VTL0 idle" mode: Switches to the idle thread, and the idle thread enters VTL0 with
        /// scheduler ticks off.
        IDLE_TO_VTL0 = 2,
    }
}

impl EnterMode {
    const fn into_bits(self) -> u8 {
        self.0
    }

    const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

/// Controls how to enter VTL0.
#[bitfield(u8)]
pub struct EnterModes {
    /// [`Mode`] used when entering VTL0 the first time.
    #[bits(4)]
    pub first: EnterMode,
    /// [`Mode`] used when interrupted from the previous enter to VTL0.
    #[bits(4)]
    pub second: EnterMode,
}

/// The register values returned from a TDG.VP.ENTER call. These are readable
/// via mmaping the mshv_vtl driver inside `hcl_run`, and returned on a run_vp
/// ioctl exit. See the TDX ABI specification for output operands for
/// TDG.VP.ENTER.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct tdx_tdg_vp_enter_exit_info {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct tdx_vp_state_flags {
    /// Issue a cache flush for a WBINVD before calling VP.ENTER.
    pub wbinvd: bool,
    /// Issue a cache flush for a WBNOINVD before calling VP.ENTER.
    pub wbnoinvd: bool,
    #[bits(62)]
    reserved: u64,
}

/// Additional VP state that is save/restored across TDG.VP.ENTER.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct tdx_vp_state {
    pub msr_kernel_gs_base: u64,
    pub msr_star: u64,
    pub msr_lstar: u64,
    pub msr_sfmask: u64,
    pub msr_xss: u64,
    pub cr2: u64,
    pub msr_tsc_aux: u64,
    pub flags: tdx_vp_state_flags,
}

/// L2 TSC deadline state for current VP.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct tdx_l2_tsc_deadline_state {
    /// Timer deadline value in absolute virtual TSC units for this processor.
    pub deadline: u64,
    /// Controls whether the TSC deadline should be updated.
    ///
    /// When set to 1, the `mshv_vtl` driver will issue a `TDG.VP.WR` call to
    /// update the TSC deadline during the next entry into lower VTL.
    pub update_deadline: u8,
    pub pad: [u8; 7],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct tdx_vp_context {
    pub exit_info: tdx_tdg_vp_enter_exit_info,
    pub pad1: [u8; 48],
    pub vp_state: tdx_vp_state,
    pub pad2: [u8; 32],
    pub entry_rcx: x86defs::tdx::TdxVmFlags,
    pub gpr_list: x86defs::tdx::TdxL2EnterGuestState,
    pub l2_tsc_deadline: tdx_l2_tsc_deadline_state,
    pub pad3: [u8; 80],
    pub fx_state: x86defs::xsave::Fxsave,
    pub pad4: [u8; 16],
}

const _: () = assert!(core::mem::offset_of!(tdx_vp_context, gpr_list) + 272 == 512);
const _: () = assert!(size_of::<tdx_vp_context>() == 1024);

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct hcl_kick_cpus_flags {
    #[bits(1)]
    pub wait_for_other_cpus: bool,
    #[bits(1)]
    pub cancel_run: bool,
    #[bits(62)]
    reserved: u64,
}

#[repr(C)]
pub struct hcl_kick_cpus {
    pub len: u64,
    pub cpu_mask: *const u8,
    pub flags: hcl_kick_cpus_flags,
}
