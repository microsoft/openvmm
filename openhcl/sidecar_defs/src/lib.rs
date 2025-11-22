// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the sidecar kernel.

#![no_std]
#![forbid(unsafe_code)]

use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use hvdef::HvMessage;
use hvdef::HvStatus;
use hvdef::hypercall::HvInputVtl;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Starting state for each CPU, supplied by `openhcl_boot`.
/// For various reasons, `openhcl_boot` may have decided to
/// instruct the linux kernel to start additional CPUs (notably,
/// in the event that we know we're about to spawn tasks on those
/// CPUs right away to handle device interrupts).
#[repr(C)]
#[derive(FromZeros, Immutable, KnownLayout, Debug, Clone)]
pub struct PerCpuState {
    /// Whether the per-CPU state is specified, since `NUM_CPUS_SUPPORTED_FOR_PER_CPU_STATE`
    /// is less than the maximum number of CPUs supported by OpenHCL and also by sidecar.
    pub per_cpu_state_specified: bool,

    /// Whether the CPU should be started by the sidecar kernel. If false,
    /// the CPU will be started by (or remain with) the main kernel instead.
    pub sidecar_starts_cpu: [bool; NUM_CPUS_SUPPORTED_FOR_PER_CPU_STATE],
}

/// Sidecar start input parameters.
#[repr(C, align(4096))]
#[derive(FromZeros, Immutable, KnownLayout)]
pub struct SidecarParams {
    /// The physical address of the x86-64 hypercall page.
    pub hypercall_page: u64,
    /// If true, enabling serial logging.
    pub enable_logging: bool,
    /// The number of valid nodes in `nodes`.
    pub node_count: u32,
    /// The node-specific input parameters.
    pub nodes: [SidecarNodeParams; MAX_NODES],
    /// The initial CPU state for each CPU.
    pub initial_state: PerCpuState,
}

/// Node-specific input parameters.
#[repr(C)]
#[derive(FromZeros, Immutable, KnownLayout)]
pub struct SidecarNodeParams {
    /// The physical address of the beginning of the reserved memory for this
    /// node. Must be page aligned.
    pub memory_base: u64,
    /// The size of the reserved memory region. Must be a page multiple.
    pub memory_size: u64,
    /// The base VP for this node.
    pub base_vp: u32,
    /// The number of VPs in the node.
    pub vp_count: u32,
}

/// The maximum number of supported sidecar nodes.
pub const MAX_NODES: usize = 128;
/// The maximum number of supported sidecar CPUs.
/// This odd number is chosen so that `SidecarParams` still
/// fits within a single page.
pub const NUM_CPUS_SUPPORTED_FOR_PER_CPU_STATE: usize = 1000;

const _: () = assert!(size_of::<SidecarParams>() <= PAGE_SIZE);

/// The output of the sidecar kernel boot process.
#[repr(C)]
#[derive(FromZeros, Immutable, KnownLayout)]
pub struct SidecarOutput {
    /// The boot error. This is only set if the entry point returns false.
    pub error: CommandError,
    /// The per-node output information.
    pub nodes: [SidecarNodeOutput; MAX_NODES],
}

/// The per-node output of the sidecar kernel boot process.
#[repr(C)]
#[derive(FromZeros, Immutable, KnownLayout)]
pub struct SidecarNodeOutput {
    /// The physical address of the control page for the node.
    pub control_page: u64,
    /// The base physical address of the per-VP pages for the node.
    pub shmem_pages_base: u64,
    /// The size of the VP page region.
    pub shmem_pages_size: u64,
}

const _: () = assert!(size_of::<SidecarOutput>() <= PAGE_SIZE);

/// The page size for all sidecar objects.
pub const PAGE_SIZE: usize = 4096;

/// The per-node control page, which is used to communicate between the sidecar
/// kernel and the main kernel sidecar kernel driver.
#[repr(C, align(4096))]
pub struct ControlPage {
    /// The node index.
    pub index: AtomicU32,
    /// The base CPU of the node.
    pub base_cpu: AtomicU32,
    /// The number of CPUs in the node.
    pub cpu_count: AtomicU32,
    /// The vector the driver should IPI to wake up a sidecar CPU.
    pub request_vector: AtomicU32,
    /// The APIC ID of the CPU that the sidecar CPU should IPI to wake up the
    /// driver.
    pub response_cpu: AtomicU32,
    /// The vector the sidecar CPU should IPI to wake up the driver.
    pub response_vector: AtomicU32,
    /// If non-zero, then a sidecar CPU has a message for the driver.
    pub needs_attention: AtomicU32,
    /// Reserved.
    pub reserved: [u8; 36],
    /// The per-CPU status.
    pub cpu_status: [AtomicU8; 4032],
}

const _: () = assert!(size_of::<ControlPage>() == PAGE_SIZE);

open_enum::open_enum! {
    /// The CPU status.
    pub enum CpuStatus: u8 {
        /// The CPU is not running in the sidecar kernel.
        REMOVED = 0,
        /// The CPU is idle, having completed any previous commands.
        IDLE = 1,
        /// The CPU is running a command.
        RUN = 2,
        /// The CPU is being asked to stop running a command.
        STOP = 3,
        /// The CPU is being asked to terminate.
        REMOVE = 4,
    }
}

/// The number of reserved pages required for each VP.
// 1. pml4
// 2. pdpt
// 3. pd
// 4. pt
// 5. globals
// 6. vp assist page
// 7. hypercall input page
// 8. hypercall output page
pub const PER_VP_PAGES: usize = 8 + STACK_PAGES;

/// The number of per-VP shared-memory pages.
// 1. command page
// 2. register page
pub const PER_VP_SHMEM_PAGES: usize = 2;

/// The number of pages in the per-VP stack.
pub const STACK_PAGES: usize = 3;

/// The required memory (in bytes) for a node.
pub const fn required_memory(vp_count: u32) -> usize {
    // Control page + per-VP pages.
    (1 + (PER_VP_SHMEM_PAGES + PER_VP_PAGES) * vp_count as usize) * PAGE_SIZE
}

/// The sidecar command page, containing command requests and responses.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CommandPage {
    /// The command to run.
    pub command: SidecarCommand,
    /// If non-zero, the command failed.
    pub has_error: u8,
    /// Padding bytes.
    pub padding: [u8; 11],
    /// The current CPU register state.
    pub cpu_context: CpuContextX64,
    /// The intercept message from the last VP exit.
    pub intercept_message: HvMessage,
    /// The error, if `has_error` is non-zero.
    pub error: CommandError,
    /// The request data for the command.
    pub request_data: [u128; REQUEST_DATA_SIZE / size_of::<u128>()],
    /// Reserved.
    pub reserved: [u64; 190],
}

const REQUEST_DATA_SIZE: usize = 64 * size_of::<u128>();

/// A string error.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CommandError {
    /// The length of the error string, in bytes.
    pub len: u8,
    /// The error string, encoded as UTF-8, containing `len` bytes.
    pub buf: [u8; 255],
}

const _: () = assert!(size_of::<CommandPage>() == PAGE_SIZE);

open_enum! {
    /// The sidecar command.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SidecarCommand: u32 {
        /// No command.
        NONE = 0,
        /// Run the VP until cancelled or an intercept occurs.
        RUN_VP = 1,
        /// Gets VP registers.
        GET_VP_REGISTERS = 2,
        /// Sets VP registers.
        SET_VP_REGISTERS = 3,
        /// Translates a guest virtual address.
        TRANSLATE_GVA = 4,
    }
}

/// A request and response for [`SidecarCommand::GET_VP_REGISTERS`] or
/// [`SidecarCommand::SET_VP_REGISTERS`].
///
/// Followed by an array of [`hvdef::hypercall::HvRegisterAssoc`], which are
/// updated in place for the get request.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GetSetVpRegisterRequest {
    /// The number of registers to get.
    pub count: u16,
    /// The target VTL.
    pub target_vtl: HvInputVtl,
    /// Reserved.
    pub rsvd: u8,
    /// The hypervisor result.
    pub status: HvStatus,
    /// Reserved.
    pub rsvd2: [u8; 10],
    /// Alignment field.
    pub regs: [hvdef::hypercall::HvRegisterAssoc; 0],
}

/// The maximum number of registers that can be requested in a single
/// [`SidecarCommand::GET_VP_REGISTERS`] or
/// [`SidecarCommand::SET_VP_REGISTERS`].
pub const MAX_GET_SET_VP_REGISTERS: usize = (REQUEST_DATA_SIZE
    - size_of::<GetSetVpRegisterRequest>())
    / size_of::<hvdef::hypercall::HvRegisterAssoc>();

/// A request for [`SidecarCommand::TRANSLATE_GVA`].
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TranslateGvaRequest {
    /// The guest virtual address page number.
    pub gvn: u64,
    /// The control flags.
    pub control_flags: hvdef::hypercall::TranslateGvaControlFlagsX64,
}

/// A response for [`SidecarCommand::TRANSLATE_GVA`].
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TranslateGvaResponse {
    /// The hypervisor result.
    pub status: HvStatus,
    /// Reserved.
    pub rsvd: [u16; 7],
    /// The output of the translation.
    pub output: hvdef::hypercall::TranslateVirtualAddressExOutputX64,
}

/// A response for [`SidecarCommand::RUN_VP`].
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RunVpResponse {
    /// If true, the VP was stopped due to an intercept.
    pub intercept: u8,
}

/// The CPU context for x86-64.
#[repr(C, align(16))]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CpuContextX64 {
    /// The general purpose registers, in the usual order, except CR2 is in
    /// RSP's position.
    pub gps: [u64; 16],
    /// The `fxsave` state.
    pub fx_state: x86defs::xsave::Fxsave,
    /// Reserved.
    pub reserved: [u8; 384],
}

impl CpuContextX64 {
    /// The index of the RAX register.
    pub const RAX: usize = 0;
    /// The index of the RCX register.
    pub const RCX: usize = 1;
    /// The index of the RDX register.
    pub const RDX: usize = 2;
    /// The index of the RBX register.
    pub const RBX: usize = 3;
    /// The index of the CR2 register.
    pub const CR2: usize = 4;
    /// The index of the RBP register.
    pub const RBP: usize = 5;
    /// The index of the RSI register.
    pub const RSI: usize = 6;
    /// The index of the RDI register.
    pub const RDI: usize = 7;
    /// The index of the R8 register.
    pub const R8: usize = 8;
    /// The index of the R9 register.
    pub const R9: usize = 9;
    /// The index of the R10 register.
    pub const R10: usize = 10;
    /// The index of the R11 register.
    pub const R11: usize = 11;
    /// The index of the R12 register.
    pub const R12: usize = 12;
    /// The index of the R13 register.
    pub const R13: usize = 13;
    /// The index of the R14 register.
    pub const R14: usize = 14;
    /// The index of the R15 register.
    pub const R15: usize = 15;
}
