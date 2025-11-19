// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Microsoft hypervisor definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

pub mod vbs;

use bitfield_struct::bitfield;
use core::fmt::Debug;
use core::mem::size_of;
use open_enum::open_enum;
use static_assertions::const_assert;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const HV_PAGE_SIZE: u64 = 4096;
pub const HV_PAGE_SIZE_USIZE: usize = 4096;
pub const HV_PAGE_SHIFT: u64 = 12;

pub const HV_PARTITION_ID_SELF: u64 = u64::MAX;
pub const HV_VP_INDEX_SELF: u32 = 0xfffffffe;

pub const HV_CPUID_FUNCTION_VERSION_AND_FEATURES: u32 = 0x00000001;
pub const HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION: u32 = 0x40000000;
pub const HV_CPUID_FUNCTION_HV_INTERFACE: u32 = 0x40000001;
pub const HV_CPUID_FUNCTION_MS_HV_VERSION: u32 = 0x40000002;
pub const HV_CPUID_FUNCTION_MS_HV_FEATURES: u32 = 0x40000003;
pub const HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION: u32 = 0x40000004;
pub const HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS: u32 = 0x40000005;
pub const HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES: u32 = 0x40000006;
pub const HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION: u32 = 0x4000000C;

pub const VIRTUALIZATION_STACK_CPUID_VENDOR: u32 = 0x40000080;
pub const VIRTUALIZATION_STACK_CPUID_INTERFACE: u32 = 0x40000081;
pub const VIRTUALIZATION_STACK_CPUID_PROPERTIES: u32 = 0x40000082;

/// The result of querying the VIRTUALIZATION_STACK_CPUID_PROPERTIES leaf.
///
/// The current partition is considered "portable": the virtualization stack may
/// attempt to bring up the partition on another physical machine.
pub const VS1_PARTITION_PROPERTIES_EAX_IS_PORTABLE: u32 = 0x000000001;
/// The current partition has a synthetic debug device available to it.
pub const VS1_PARTITION_PROPERTIES_EAX_DEBUG_DEVICE_PRESENT: u32 = 0x000000002;
/// Extended I/O APIC RTEs are supported for the current partition.
pub const VS1_PARTITION_PROPERTIES_EAX_EXTENDED_IOAPIC_RTE: u32 = 0x000000004;
/// Confidential VMBus is available.
pub const VS1_PARTITION_PROPERTIES_EAX_CONFIDENTIAL_VMBUS_AVAILABLE: u32 = 0x000000008;

/// SMCCC UID for the Microsoft Hypervisor.
pub const VENDOR_HYP_UID_MS_HYPERVISOR: [u32; 4] = [0x4d32ba58, 0xcd244764, 0x8eef6c75, 0x16597024];

#[bitfield(u64)]
pub struct HvPartitionPrivilege {
    // access to virtual msrs
    pub access_vp_runtime_msr: bool,
    pub access_partition_reference_counter: bool,
    pub access_synic_msrs: bool,
    pub access_synthetic_timer_msrs: bool,
    pub access_apic_msrs: bool,
    pub access_hypercall_msrs: bool,
    pub access_vp_index: bool,
    pub access_reset_msr: bool,
    pub access_stats_msr: bool,
    pub access_partition_reference_tsc: bool,
    pub access_guest_idle_msr: bool,
    pub access_frequency_msrs: bool,
    pub access_debug_msrs: bool,
    pub access_reenlightenment_ctrls: bool,
    pub access_root_scheduler_msr: bool,
    pub access_tsc_invariant_controls: bool,
    _reserved1: u16,

    // Access to hypercalls
    pub create_partitions: bool,
    pub access_partition_id: bool,
    pub access_memory_pool: bool,
    pub adjust_message_buffers: bool,
    pub post_messages: bool,
    pub signal_events: bool,
    pub create_port: bool,
    pub connect_port: bool,
    pub access_stats: bool,
    #[bits(2)]
    _reserved2: u64,
    pub debugging: bool,
    pub cpu_management: bool,
    pub configure_profiler: bool,
    pub access_vp_exit_tracing: bool,
    pub enable_extended_gva_ranges_flush_va_list: bool,
    pub access_vsm: bool,
    pub access_vp_registers: bool,
    _unused_bit: bool,
    pub fast_hypercall_output: bool,
    pub enable_extended_hypercalls: bool,
    pub start_virtual_processor: bool,
    pub isolation: bool,
    #[bits(9)]
    _reserved3: u64,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvPartitionIsolationType: u8 {
        NONE = 0,
        VBS = 1,
        SNP = 2,
        TDX = 3,
    }
}

#[bitfield(u128)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvFeatures {
    #[bits(64)]
    pub privileges: HvPartitionPrivilege,

    #[bits(4)]
    pub max_supported_cstate: u32,
    pub hpet_needed_for_c3_power_state_deprecated: bool,
    pub invariant_mperf_available: bool,
    pub supervisor_shadow_stack_available: bool,
    pub arch_pmu_available: bool,
    pub exception_trap_intercept_available: bool,
    #[bits(23)]
    reserved: u32,

    pub mwait_available_deprecated: bool,
    pub guest_debugging_available: bool,
    pub performance_monitors_available: bool,
    pub cpu_dynamic_partitioning_available: bool,
    pub xmm_registers_for_fast_hypercall_available: bool,
    pub guest_idle_available: bool,
    pub hypervisor_sleep_state_support_available: bool,
    pub numa_distance_query_available: bool,
    pub frequency_regs_available: bool,
    pub synthetic_machine_check_available: bool,
    pub guest_crash_regs_available: bool,
    pub debug_regs_available: bool,
    pub npiep1_available: bool,
    pub disable_hypervisor_available: bool,
    pub extended_gva_ranges_for_flush_virtual_address_list_available: bool,
    pub fast_hypercall_output_available: bool,
    pub svm_features_available: bool,
    pub sint_polling_mode_available: bool,
    pub hypercall_msr_lock_available: bool,
    pub direct_synthetic_timers: bool,
    pub register_pat_available: bool,
    pub register_bndcfgs_available: bool,
    pub watchdog_timer_available: bool,
    pub synthetic_time_unhalted_timer_available: bool,
    pub device_domains_available: bool,    // HDK only.
    pub s1_device_domains_available: bool, // HDK only.
    pub lbr_available: bool,
    pub ipt_available: bool,
    pub cross_vtl_flush_available: bool,
    pub idle_spec_ctrl_available: bool,
    pub translate_gva_flags_available: bool,
    pub apic_eoi_intercept_available: bool,
}

impl HvFeatures {
    pub fn from_cpuid(cpuid: [u32; 4]) -> Self {
        zerocopy::transmute!(cpuid)
    }

    pub fn into_cpuid(self) -> [u32; 4] {
        zerocopy::transmute!(self)
    }
}

#[bitfield(u128)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvEnlightenmentInformation {
    pub use_hypercall_for_address_space_switch: bool,
    pub use_hypercall_for_local_flush: bool,
    pub use_hypercall_for_remote_flush_and_local_flush_entire: bool,
    pub use_apic_msrs: bool,
    pub use_hv_register_for_reset: bool,
    pub use_relaxed_timing: bool,
    pub use_dma_remapping_deprecated: bool,
    pub use_interrupt_remapping_deprecated: bool,
    pub use_x2_apic_msrs: bool,
    pub deprecate_auto_eoi: bool,
    pub use_synthetic_cluster_ipi: bool,
    pub use_ex_processor_masks: bool,
    pub nested: bool,
    pub use_int_for_mbec_system_calls: bool,
    pub use_vmcs_enlightenments: bool,
    pub use_synced_timeline: bool,
    pub core_scheduler_requested: bool,
    pub use_direct_local_flush_entire: bool,
    pub no_non_architectural_core_sharing: bool,
    pub use_x2_apic: bool,
    pub restore_time_on_resume: bool,
    pub use_hypercall_for_mmio_access: bool,
    pub use_gpa_pinning_hypercall: bool,
    pub wake_vps: bool,
    _reserved: u8,
    pub long_spin_wait_count: u32,
    #[bits(7)]
    pub implemented_physical_address_bits: u32,
    #[bits(25)]
    _reserved1: u32,
    _reserved2: u32,
}

impl HvEnlightenmentInformation {
    pub fn from_cpuid(cpuid: [u32; 4]) -> Self {
        zerocopy::transmute!(cpuid)
    }

    pub fn into_cpuid(self) -> [u32; 4] {
        zerocopy::transmute!(self)
    }
}

#[bitfield(u128)]
pub struct HvHardwareFeatures {
    pub apic_overlay_assist_in_use: bool,
    pub msr_bitmaps_in_use: bool,
    pub architectural_performance_counters_in_use: bool,
    pub second_level_address_translation_in_use: bool,
    pub dma_remapping_in_use: bool,
    pub interrupt_remapping_in_use: bool,
    pub memory_patrol_scrubber_present: bool,
    pub dma_protection_in_use: bool,
    pub hpet_requested: bool,
    pub synthetic_timers_volatile: bool,
    #[bits(4)]
    pub hypervisor_level: u32,
    pub physical_destination_mode_required: bool,
    pub use_vmfunc_for_alias_map_switch: bool,
    pub hv_register_for_memory_zeroing_supported: bool,
    pub unrestricted_guest_supported: bool,
    pub rdt_afeatures_supported: bool,
    pub rdt_mfeatures_supported: bool,
    pub child_perfmon_pmu_supported: bool,
    pub child_perfmon_lbr_supported: bool,
    pub child_perfmon_ipt_supported: bool,
    pub apic_emulation_supported: bool,
    pub child_x2_apic_recommended: bool,
    pub hardware_watchdog_reserved: bool,
    pub device_access_tracking_supported: bool,
    pub hardware_gpa_access_tracking_supported: bool,
    #[bits(4)]
    _reserved: u32,

    pub device_domain_input_width: u8,
    #[bits(24)]
    _reserved1: u32,
    _reserved2: u32,
    _reserved3: u32,
}

#[bitfield(u128)]
pub struct HvIsolationConfiguration {
    pub paravisor_present: bool,
    #[bits(31)]
    pub _reserved0: u32,

    #[bits(4)]
    pub isolation_type: u8,
    _reserved11: bool,
    pub shared_gpa_boundary_active: bool,
    #[bits(6)]
    pub shared_gpa_boundary_bits: u8,
    #[bits(20)]
    _reserved12: u32,
    _reserved2: u32,
    _reserved3: u32,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HypercallCode: u16 {
        #![expect(non_upper_case_globals)]

        HvCallSwitchVirtualAddressSpace = 0x0001,
        HvCallFlushVirtualAddressSpace = 0x0002,
        HvCallFlushVirtualAddressList = 0x0003,
        HvCallNotifyLongSpinWait = 0x0008,
        HvCallInvokeHypervisorDebugger = 0x000a,
        HvCallSendSyntheticClusterIpi = 0x000b,
        HvCallModifyVtlProtectionMask = 0x000c,
        HvCallEnablePartitionVtl = 0x000d,
        HvCallEnableVpVtl = 0x000f,
        HvCallVtlCall = 0x0011,
        HvCallVtlReturn = 0x0012,
        HvCallFlushVirtualAddressSpaceEx = 0x0013,
        HvCallFlushVirtualAddressListEx = 0x0014,
        HvCallSendSyntheticClusterIpiEx = 0x0015,
        HvCallInstallIntercept = 0x004d,
        HvCallGetVpRegisters = 0x0050,
        HvCallSetVpRegisters = 0x0051,
        HvCallTranslateVirtualAddress = 0x0052,
        HvCallPostMessage = 0x005C,
        HvCallSignalEvent = 0x005D,
        HvCallOutputDebugCharacter = 0x0071,
        HvCallGetSystemProperty = 0x007b,
        HvCallRetargetDeviceInterrupt = 0x007e,
        HvCallNotifyPartitionEvent = 0x0087,
        HvCallAssertVirtualInterrupt = 0x0094,
        HvCallStartVirtualProcessor = 0x0099,
        HvCallGetVpIndexFromApicId = 0x009A,
        HvCallTranslateVirtualAddressEx = 0x00AC,
        HvCallCheckForIoIntercept = 0x00ad,
        HvCallFlushGuestPhysicalAddressSpace = 0x00AF,
        HvCallFlushGuestPhysicalAddressList = 0x00B0,
        HvCallSignalEventDirect = 0x00C0,
        HvCallPostMessageDirect = 0x00C1,
        HvCallCheckSparseGpaPageVtlAccess = 0x00D4,
        HvCallAcceptGpaPages = 0x00D9,
        HvCallModifySparseGpaPageHostVisibility = 0x00DB,
        HvCallMemoryMappedIoRead = 0x0106,
        HvCallMemoryMappedIoWrite = 0x0107,
        HvCallPinGpaPageRanges = 0x0112,
        HvCallUnpinGpaPageRanges = 0x0113,
        HvCallQuerySparseGpaPageHostVisibility = 0x011C,

        // Extended hypercalls.
        HvExtCallQueryCapabilities = 0x8001,

        // VBS guest calls.
        HvCallVbsVmCallReport = 0xC001,
    }
}

pub const HV_X64_MSR_GUEST_OS_ID: u32 = 0x40000000;
pub const HV_X64_MSR_HYPERCALL: u32 = 0x40000001;
pub const HV_X64_MSR_VP_INDEX: u32 = 0x40000002;
pub const HV_X64_MSR_TIME_REF_COUNT: u32 = 0x40000020;
pub const HV_X64_MSR_REFERENCE_TSC: u32 = 0x40000021;
pub const HV_X64_MSR_TSC_FREQUENCY: u32 = 0x40000022;
pub const HV_X64_MSR_APIC_FREQUENCY: u32 = 0x40000023;
pub const HV_X64_MSR_EOI: u32 = 0x40000070;
pub const HV_X64_MSR_ICR: u32 = 0x40000071;
pub const HV_X64_MSR_TPR: u32 = 0x40000072;
pub const HV_X64_MSR_VP_ASSIST_PAGE: u32 = 0x40000073;
pub const HV_X64_MSR_SCONTROL: u32 = 0x40000080;
pub const HV_X64_MSR_SVERSION: u32 = 0x40000081;
pub const HV_X64_MSR_SIEFP: u32 = 0x40000082;
pub const HV_X64_MSR_SIMP: u32 = 0x40000083;
pub const HV_X64_MSR_EOM: u32 = 0x40000084;
pub const HV_X64_MSR_SINT0: u32 = 0x40000090;
pub const HV_X64_MSR_SINT1: u32 = 0x40000091;
pub const HV_X64_MSR_SINT2: u32 = 0x40000092;
pub const HV_X64_MSR_SINT3: u32 = 0x40000093;
pub const HV_X64_MSR_SINT4: u32 = 0x40000094;
pub const HV_X64_MSR_SINT5: u32 = 0x40000095;
pub const HV_X64_MSR_SINT6: u32 = 0x40000096;
pub const HV_X64_MSR_SINT7: u32 = 0x40000097;
pub const HV_X64_MSR_SINT8: u32 = 0x40000098;
pub const HV_X64_MSR_SINT9: u32 = 0x40000099;
pub const HV_X64_MSR_SINT10: u32 = 0x4000009a;
pub const HV_X64_MSR_SINT11: u32 = 0x4000009b;
pub const HV_X64_MSR_SINT12: u32 = 0x4000009c;
pub const HV_X64_MSR_SINT13: u32 = 0x4000009d;
pub const HV_X64_MSR_SINT14: u32 = 0x4000009e;
pub const HV_X64_MSR_SINT15: u32 = 0x4000009f;
pub const HV_X64_MSR_STIMER0_CONFIG: u32 = 0x400000b0;
pub const HV_X64_MSR_STIMER0_COUNT: u32 = 0x400000b1;
pub const HV_X64_MSR_STIMER1_CONFIG: u32 = 0x400000b2;
pub const HV_X64_MSR_STIMER1_COUNT: u32 = 0x400000b3;
pub const HV_X64_MSR_STIMER2_CONFIG: u32 = 0x400000b4;
pub const HV_X64_MSR_STIMER2_COUNT: u32 = 0x400000b5;
pub const HV_X64_MSR_STIMER3_CONFIG: u32 = 0x400000b6;
pub const HV_X64_MSR_STIMER3_COUNT: u32 = 0x400000b7;
pub const HV_X64_MSR_GUEST_IDLE: u32 = 0x400000F0;
pub const HV_X64_MSR_GUEST_CRASH_P0: u32 = 0x40000100;
pub const HV_X64_MSR_GUEST_CRASH_P1: u32 = 0x40000101;
pub const HV_X64_MSR_GUEST_CRASH_P2: u32 = 0x40000102;
pub const HV_X64_MSR_GUEST_CRASH_P3: u32 = 0x40000103;
pub const HV_X64_MSR_GUEST_CRASH_P4: u32 = 0x40000104;
pub const HV_X64_MSR_GUEST_CRASH_CTL: u32 = 0x40000105;

pub const HV_X64_GUEST_CRASH_PARAMETER_MSRS: usize = 5;

/// A hypervisor status code.
///
/// The non-success status codes are defined in [`HvError`].
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
#[repr(transparent)]
pub struct HvStatus(pub u16);

impl HvStatus {
    /// The success status code.
    pub const SUCCESS: Self = Self(0);

    /// Returns `Ok(())` if this is `HvStatus::SUCCESS`, otherwise returns an
    /// `Err(err)` where `err` is the corresponding `HvError`.
    pub fn result(self) -> HvResult<()> {
        if let Ok(err) = self.0.try_into() {
            Err(HvError(err))
        } else {
            Ok(())
        }
    }

    /// Returns true if this is `HvStatus::SUCCESS`.
    pub fn is_ok(self) -> bool {
        self == Self::SUCCESS
    }

    /// Returns true if this is not `HvStatus::SUCCESS`.
    pub fn is_err(self) -> bool {
        self != Self::SUCCESS
    }

    const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    const fn into_bits(self) -> u16 {
        self.0
    }
}

impl From<Result<(), HvError>> for HvStatus {
    fn from(err: Result<(), HvError>) -> Self {
        err.err().map_or(Self::SUCCESS, |err| Self(err.0.get()))
    }
}

impl Debug for HvStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.result() {
            Ok(()) => f.write_str("Success"),
            Err(err) => Debug::fmt(&err, f),
        }
    }
}

/// An [`HvStatus`] value representing an error.
//
// DEVNOTE: use `NonZeroU16` to get a niche optimization, since 0 is reserved
// for success.
#[derive(Copy, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct HvError(core::num::NonZeroU16);

impl From<core::num::NonZeroU16> for HvError {
    fn from(err: core::num::NonZeroU16) -> Self {
        Self(err)
    }
}

impl Debug for HvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.debug_name() {
            Some(name) => f.pad(name),
            None => Debug::fmt(&self.0.get(), f),
        }
    }
}

impl core::fmt::Display for HvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.doc_str() {
            Some(s) => f.write_str(s),
            None => write!(f, "Hypervisor error {:#06x}", self.0),
        }
    }
}

impl core::error::Error for HvError {}

macro_rules! hv_error {
    ($ty:ty, $(#[doc = $doc:expr] $ident:ident = $val:expr),* $(,)?) => {

        #[expect(non_upper_case_globals)]
        impl $ty {
            $(
                #[doc = $doc]
                pub const $ident: Self = Self(core::num::NonZeroU16::new($val).unwrap());
            )*

            fn debug_name(&self) -> Option<&'static str> {
                Some(match self.0.get() {
                    $(
                        $val => stringify!($ident),
                    )*
                    _ => return None,
                })
            }

            fn doc_str(&self) -> Option<&'static str> {
                Some(match self.0.get() {
                    $(
                        $val => const { $doc.trim_ascii() },
                    )*
                    _ => return None,
                })
            }
        }
    };
}

// DEVNOTE: the doc comments here are also used as the runtime error strings.
hv_error! {
    HvError,
    /// Invalid hypercall code
    InvalidHypercallCode = 0x0002,
    /// Invalid hypercall input
    InvalidHypercallInput = 0x0003,
    /// Invalid alignment
    InvalidAlignment = 0x0004,
    /// Invalid parameter
    InvalidParameter = 0x0005,
    /// Access denied
    AccessDenied = 0x0006,
    /// Invalid partition state
    InvalidPartitionState = 0x0007,
    /// Operation denied
    OperationDenied = 0x0008,
    /// Unknown property
    UnknownProperty = 0x0009,
    /// Property value out of range
    PropertyValueOutOfRange = 0x000A,
    /// Insufficient memory
    InsufficientMemory = 0x000B,
    /// Partition too deep
    PartitionTooDeep = 0x000C,
    /// Invalid partition ID
    InvalidPartitionId = 0x000D,
    /// Invalid VP index
    InvalidVpIndex = 0x000E,
    /// Not found
    NotFound = 0x0010,
    /// Invalid port ID
    InvalidPortId = 0x0011,
    /// Invalid connection ID
    InvalidConnectionId = 0x0012,
    /// Insufficient buffers
    InsufficientBuffers = 0x0013,
    /// Not acknowledged
    NotAcknowledged = 0x0014,
    /// Invalid VP state
    InvalidVpState = 0x0015,
    /// Acknowledged
    Acknowledged = 0x0016,
    /// Invalid save restore state
    InvalidSaveRestoreState = 0x0017,
    /// Invalid SynIC state
    InvalidSynicState = 0x0018,
    /// Object in use
    ObjectInUse = 0x0019,
    /// Invalid proximity domain info
    InvalidProximityDomainInfo = 0x001A,
    /// No data
    NoData = 0x001B,
    /// Inactive
    Inactive = 0x001C,
    /// No resources
    NoResources = 0x001D,
    /// Feature unavailable
    FeatureUnavailable = 0x001E,
    /// Partial packet
    PartialPacket = 0x001F,
    /// Processor feature not supported
    ProcessorFeatureNotSupported = 0x0020,
    /// Processor cache line flush size incompatible
    ProcessorCacheLineFlushSizeIncompatible = 0x0030,
    /// Insufficient buffer
    InsufficientBuffer = 0x0033,
    /// Incompatible processor
    IncompatibleProcessor = 0x0037,
    /// Insufficient device domains
    InsufficientDeviceDomains = 0x0038,
    /// CPUID feature validation error
    CpuidFeatureValidationError = 0x003C,
    /// CPUID XSAVE feature validation error
    CpuidXsaveFeatureValidationError = 0x003D,
    /// Processor startup timeout
    ProcessorStartupTimeout = 0x003E,
    /// SMX enabled
    SmxEnabled = 0x003F,
    /// Invalid LP index
    InvalidLpIndex = 0x0041,
    /// Invalid register value
    InvalidRegisterValue = 0x0050,
    /// Invalid VTL state
    InvalidVtlState = 0x0051,
    /// NX not detected
    NxNotDetected = 0x0055,
    /// Invalid device ID
    InvalidDeviceId = 0x0057,
    /// Invalid device state
    InvalidDeviceState = 0x0058,
    /// Pending page requests
    PendingPageRequests = 0x0059,
    /// Page request invalid
    PageRequestInvalid = 0x0060,
    /// Key already exists
    KeyAlreadyExists = 0x0065,
    /// Device already in domain
    DeviceAlreadyInDomain = 0x0066,
    /// Invalid CPU group ID
    InvalidCpuGroupId = 0x006F,
    /// Invalid CPU group state
    InvalidCpuGroupState = 0x0070,
    /// Operation failed
    OperationFailed = 0x0071,
    /// Not allowed with nested virtualization active
    NotAllowedWithNestedVirtActive = 0x0072,
    /// Insufficient root memory
    InsufficientRootMemory = 0x0073,
    /// Event buffer already freed
    EventBufferAlreadyFreed = 0x0074,
    /// The specified timeout expired before the operation completed.
    Timeout = 0x0078,
    /// The VTL specified for the operation is already in an enabled state.
    VtlAlreadyEnabled = 0x0086,
    /// Unknown register name
    UnknownRegisterName = 0x0087,
}

/// A useful result type for hypervisor operations.
pub type HvResult<T> = Result<T, HvError>;

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Vtl {
    Vtl0 = 0,
    Vtl1 = 1,
    Vtl2 = 2,
}

impl TryFrom<u8> for Vtl {
    type Error = HvError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Vtl0,
            1 => Self::Vtl1,
            2 => Self::Vtl2,
            _ => return Err(HvError::InvalidParameter),
        })
    }
}

impl From<Vtl> for u8 {
    fn from(value: Vtl) -> Self {
        value as u8
    }
}

/// The contents of `HV_X64_MSR_GUEST_CRASH_CTL`
#[bitfield(u64)]
pub struct GuestCrashCtl {
    #[bits(58)]
    _reserved: u64,
    // ID of the pre-OS environment
    #[bits(3)]
    pub pre_os_id: u8,
    // Crash dump will not be captured
    #[bits(1)]
    pub no_crash_dump: bool,
    // `HV_X64_MSR_GUEST_CRASH_P3` is the GPA of the message,
    // `HV_X64_MSR_GUEST_CRASH_P4` is its length in bytes
    #[bits(1)]
    pub crash_message: bool,
    // Log contents of crash parameter system registers
    #[bits(1)]
    pub crash_notify: bool,
}

#[repr(C, align(16))]
#[derive(Copy, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct AlignedU128([u8; 16]);

impl AlignedU128 {
    pub fn as_ne_bytes(&self) -> [u8; 16] {
        self.0
    }

    pub fn from_ne_bytes(val: [u8; 16]) -> Self {
        Self(val)
    }
}

impl Debug for AlignedU128 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&u128::from_ne_bytes(self.0), f)
    }
}

impl From<u128> for AlignedU128 {
    fn from(v: u128) -> Self {
        Self(v.to_ne_bytes())
    }
}

impl From<u64> for AlignedU128 {
    fn from(v: u64) -> Self {
        (v as u128).into()
    }
}

impl From<u32> for AlignedU128 {
    fn from(v: u32) -> Self {
        (v as u128).into()
    }
}

impl From<u16> for AlignedU128 {
    fn from(v: u16) -> Self {
        (v as u128).into()
    }
}

impl From<u8> for AlignedU128 {
    fn from(v: u8) -> Self {
        (v as u128).into()
    }
}

impl From<AlignedU128> for u128 {
    fn from(v: AlignedU128) -> Self {
        u128::from_ne_bytes(v.0)
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvMessageType: u32 {
        #![expect(non_upper_case_globals)]

        HvMessageTypeNone = 0x00000000,

        HvMessageTypeUnmappedGpa = 0x80000000,
        HvMessageTypeGpaIntercept = 0x80000001,
        HvMessageTypeUnacceptedGpa = 0x80000003,
        HvMessageTypeGpaAttributeIntercept = 0x80000004,
        HvMessageTypeEnablePartitionVtlIntercept = 0x80000005,
        HvMessageTypeTimerExpired = 0x80000010,
        HvMessageTypeInvalidVpRegisterValue = 0x80000020,
        HvMessageTypeUnrecoverableException = 0x80000021,
        HvMessageTypeUnsupportedFeature = 0x80000022,
        HvMessageTypeTlbPageSizeMismatch = 0x80000023,
        HvMessageTypeIommuFault = 0x80000024,
        HvMessageTypeEventLogBufferComplete = 0x80000040,
        HvMessageTypeHypercallIntercept = 0x80000050,
        HvMessageTypeSynicEventIntercept = 0x80000060,
        HvMessageTypeSynicSintIntercept = 0x80000061,
        HvMessageTypeSynicSintDeliverable = 0x80000062,
        HvMessageTypeAsyncCallCompletion = 0x80000070,
        HvMessageTypeX64IoPortIntercept = 0x80010000,
        HvMessageTypeMsrIntercept = 0x80010001,
        HvMessageTypeX64CpuidIntercept = 0x80010002,
        HvMessageTypeExceptionIntercept = 0x80010003,
        HvMessageTypeX64ApicEoi = 0x80010004,
        HvMessageTypeX64IommuPrq = 0x80010005,
        HvMessageTypeRegisterIntercept = 0x80010006,
        HvMessageTypeX64Halt = 0x80010007,
        HvMessageTypeX64InterruptionDeliverable = 0x80010008,
        HvMessageTypeX64SipiIntercept = 0x80010009,
        HvMessageTypeX64RdtscIntercept = 0x8001000a,
        HvMessageTypeX64ApicSmiIntercept = 0x8001000b,
        HvMessageTypeArm64ResetIntercept = 0x8001000c,
        HvMessageTypeX64ApicInitSipiIntercept = 0x8001000d,
        HvMessageTypeX64ApicWriteIntercept = 0x8001000e,
        HvMessageTypeX64ProxyInterruptIntercept = 0x8001000f,
        HvMessageTypeX64IsolationCtrlRegIntercept = 0x80010010,
        HvMessageTypeX64SnpGuestRequestIntercept = 0x80010011,
        HvMessageTypeX64ExceptionTrapIntercept = 0x80010012,
        HvMessageTypeX64SevVmgexitIntercept = 0x80010013,
    }
}

impl Default for HvMessageType {
    fn default() -> Self {
        HvMessageType::HvMessageTypeNone
    }
}

pub const HV_SYNIC_INTERCEPTION_SINT_INDEX: u8 = 0;

pub const NUM_SINTS: usize = 16;
pub const NUM_TIMERS: usize = 4;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMessageHeader {
    pub typ: HvMessageType,
    pub len: u8,
    pub flags: HvMessageFlags,
    pub rsvd: u16,
    pub id: u64,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMessageFlags {
    pub message_pending: bool,
    #[bits(7)]
    _reserved: u8,
}

pub const HV_MESSAGE_SIZE: usize = size_of::<HvMessage>();
const_assert!(HV_MESSAGE_SIZE == 256);
pub const HV_MESSAGE_PAYLOAD_SIZE: usize = 240;

#[repr(C, align(16))]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMessage {
    pub header: HvMessageHeader,
    pub payload_buffer: [u8; HV_MESSAGE_PAYLOAD_SIZE],
}

impl Default for HvMessage {
    fn default() -> Self {
        Self {
            header: FromZeros::new_zeroed(),
            payload_buffer: [0; 240],
        }
    }
}

impl HvMessage {
    /// Constructs a new message. `payload` must fit into the payload field (240
    /// bytes limit).
    pub fn new(typ: HvMessageType, id: u64, payload: &[u8]) -> Self {
        let mut msg = HvMessage {
            header: HvMessageHeader {
                typ,
                len: payload.len() as u8,
                flags: HvMessageFlags::new(),
                rsvd: 0,
                id,
            },
            payload_buffer: [0; 240],
        };
        msg.payload_buffer[..payload.len()].copy_from_slice(payload);
        msg
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload_buffer[..self.header.len as usize]
    }

    pub fn as_message<T: MessagePayload>(&self) -> &T {
        // Ensure invariants are met.
        let () = T::CHECK;
        T::ref_from_prefix(&self.payload_buffer).unwrap().0
    }

    pub fn as_message_mut<T: MessagePayload>(&mut self) -> &T {
        // Ensure invariants are met.
        let () = T::CHECK;
        T::mut_from_prefix(&mut self.payload_buffer).unwrap().0
    }
}

pub trait MessagePayload: KnownLayout + Immutable + IntoBytes + FromBytes + Sized {
    /// Used to ensure this trait is only implemented on messages of the proper
    /// size and alignment.
    #[doc(hidden)]
    const CHECK: () = {
        assert!(size_of::<Self>() <= HV_MESSAGE_PAYLOAD_SIZE);
        assert!(align_of::<Self>() <= align_of::<HvMessage>());
    };
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TimerMessagePayload {
    pub timer_index: u32,
    pub reserved: u32,
    pub expiration_time: u64,
    pub delivery_time: u64,
}

pub mod hypercall {
    use super::*;
    use core::ops::RangeInclusive;
    use zerocopy::Unalign;

    /// The hypercall input value.
    #[bitfield(u64)]
    pub struct Control {
        /// The hypercall code.
        pub code: u16,
        /// If this hypercall is a fast hypercall.
        pub fast: bool,
        /// The variable header size, in qwords.
        #[bits(10)]
        pub variable_header_size: usize,
        #[bits(4)]
        _rsvd0: u8,
        /// Specifies that the hypercall should be handled by the L0 hypervisor in a nested environment.
        pub nested: bool,
        /// The element count for rep hypercalls.
        #[bits(12)]
        pub rep_count: usize,
        #[bits(4)]
        _rsvd1: u8,
        /// The first element to start processing in a rep hypercall.
        #[bits(12)]
        pub rep_start: usize,
        #[bits(4)]
        _rsvd2: u8,
    }

    /// The hypercall output value returned to the guest.
    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[must_use]
    pub struct HypercallOutput {
        #[bits(16)]
        pub call_status: HvStatus,
        pub rsvd: u16,
        #[bits(12)]
        pub elements_processed: usize,
        #[bits(20)]
        pub rsvd2: u32,
    }

    impl From<HvError> for HypercallOutput {
        fn from(e: HvError) -> Self {
            Self::new().with_call_status(Err(e).into())
        }
    }

    impl HypercallOutput {
        /// A success output with zero elements processed.
        pub const SUCCESS: Self = Self::new();

        pub fn result(&self) -> Result<(), HvError> {
            self.call_status().result()
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvRegisterAssoc {
        pub name: HvRegisterName,
        pub pad: [u32; 3],
        pub value: HvRegisterValue,
    }

    impl<N: Into<HvRegisterName>, T: Into<HvRegisterValue>> From<(N, T)> for HvRegisterAssoc {
        fn from((name, value): (N, T)) -> Self {
            Self {
                name: name.into(),
                pad: [0; 3],
                value: value.into(),
            }
        }
    }

    impl<N: Copy + Into<HvRegisterName>, T: Copy + Into<HvRegisterValue>> From<&(N, T)>
        for HvRegisterAssoc
    {
        fn from(&(name, value): &(N, T)) -> Self {
            Self {
                name: name.into(),
                pad: [0; 3],
                value: value.into(),
            }
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct MsrHypercallContents {
        pub enable: bool,
        pub locked: bool,
        #[bits(10)]
        pub reserved_p: u64,
        #[bits(52)]
        pub gpn: u64,
    }

    #[repr(C, align(8))]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PostMessage {
        pub connection_id: u32,
        pub padding: u32,
        pub message_type: u32,
        pub payload_size: u32,
        pub payload: [u8; 240],
    }

    #[repr(C, align(8))]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SignalEvent {
        pub connection_id: u32,
        pub flag_number: u16,
        pub rsvd: u16,
    }

    #[repr(C)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PostMessageDirect {
        pub partition_id: u64,
        pub vp_index: u32,
        pub vtl: u8,
        pub padding0: [u8; 3],
        pub sint: u8,
        pub padding1: [u8; 3],
        pub message: Unalign<HvMessage>,
        pub padding2: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SignalEventDirect {
        pub target_partition: u64,
        pub target_vp: u32,
        pub target_vtl: u8,
        pub target_sint: u8,
        pub flag_number: u16,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SignalEventDirectOutput {
        pub newly_signaled: u8,
        pub rsvd: [u8; 7],
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct InterruptEntry {
        pub source: HvInterruptSource,
        pub rsvd: u32,
        pub data: [u32; 2],
    }

    open_enum! {
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum HvInterruptSource: u32 {
            MSI = 1,
            IO_APIC = 2,
        }
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct InterruptTarget {
        pub vector: u32,
        pub flags: HvInterruptTargetFlags,
        pub mask_or_format: u64,
    }

    #[bitfield(u32)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvInterruptTargetFlags {
        pub multicast: bool,
        pub processor_set: bool,
        pub proxy_redirect: bool,
        #[bits(29)]
        pub reserved: u32,
    }

    pub const HV_DEVICE_INTERRUPT_TARGET_MULTICAST: u32 = 1;
    pub const HV_DEVICE_INTERRUPT_TARGET_PROCESSOR_SET: u32 = 2;
    pub const HV_DEVICE_INTERRUPT_TARGET_PROXY_REDIRECT: u32 = 4;

    pub const HV_GENERIC_SET_SPARSE_4K: u64 = 0;
    pub const HV_GENERIC_SET_ALL: u64 = 1;

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct RetargetDeviceInterrupt {
        pub partition_id: u64,
        pub device_id: u64,
        pub entry: InterruptEntry,
        pub rsvd: u64,
        pub target_header: InterruptTarget,
    }

    #[bitfield(u8)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvInputVtl {
        #[bits(4)]
        pub target_vtl_value: u8,
        pub use_target_vtl: bool,
        #[bits(3)]
        pub reserved: u8,
    }

    impl From<Vtl> for HvInputVtl {
        fn from(value: Vtl) -> Self {
            Self::from(Some(value))
        }
    }

    impl From<Option<Vtl>> for HvInputVtl {
        fn from(value: Option<Vtl>) -> Self {
            Self::new()
                .with_use_target_vtl(value.is_some())
                .with_target_vtl_value(value.map_or(0, Into::into))
        }
    }

    impl HvInputVtl {
        /// None = target current vtl
        pub fn target_vtl(&self) -> Result<Option<Vtl>, HvError> {
            if self.reserved() != 0 {
                return Err(HvError::InvalidParameter);
            }
            if self.use_target_vtl() {
                Ok(Some(self.target_vtl_value().try_into()?))
            } else {
                Ok(None)
            }
        }

        pub const CURRENT_VTL: Self = Self::new();
    }

    #[repr(C)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct GetSetVpRegisters {
        pub partition_id: u64,
        pub vp_index: u32,
        pub target_vtl: HvInputVtl,
        pub rsvd: [u8; 3],
    }

    open_enum::open_enum! {
        #[derive(Default)]
        pub enum HvGuestOsMicrosoftIds: u8 {
            UNDEFINED = 0x00,
            MSDOS = 0x01,
            WINDOWS_3X = 0x02,
            WINDOWS_9X = 0x03,
            WINDOWS_NT = 0x04,
            WINDOWS_CE = 0x05,
        }
    }

    #[bitfield(u64)]
    pub struct HvGuestOsMicrosoft {
        #[bits(40)]
        _rsvd: u64,
        #[bits(8)]
        pub os_id: u8,
        // The top bit must be zero and the least significant 15 bits holds the value of the vendor id.
        #[bits(16)]
        pub vendor_id: u16,
    }

    open_enum::open_enum! {
        #[derive(Default)]
        pub enum HvGuestOsOpenSourceType: u8 {
            UNDEFINED = 0x00,
            LINUX = 0x01,
            FREEBSD = 0x02,
            XEN = 0x03,
            ILLUMOS = 0x04,
        }
    }

    #[bitfield(u64)]
    pub struct HvGuestOsOpenSource {
        #[bits(16)]
        pub build_no: u16,
        #[bits(32)]
        pub version: u32,
        #[bits(8)]
        pub os_id: u8,
        #[bits(7)]
        pub os_type: u8,
        #[bits(1)]
        pub is_open_source: bool,
    }

    #[bitfield(u64)]
    pub struct HvGuestOsId {
        #[bits(63)]
        _rsvd: u64,
        is_open_source: bool,
    }

    impl HvGuestOsId {
        pub fn microsoft(&self) -> Option<HvGuestOsMicrosoft> {
            (!self.is_open_source()).then(|| HvGuestOsMicrosoft::from(u64::from(*self)))
        }

        pub fn open_source(&self) -> Option<HvGuestOsOpenSource> {
            (self.is_open_source()).then(|| HvGuestOsOpenSource::from(u64::from(*self)))
        }

        pub fn as_u64(&self) -> u64 {
            self.0
        }
    }

    pub const HV_INTERCEPT_ACCESS_MASK_NONE: u32 = 0x00;
    pub const HV_INTERCEPT_ACCESS_MASK_READ: u32 = 0x01;
    pub const HV_INTERCEPT_ACCESS_MASK_WRITE: u32 = 0x02;
    pub const HV_INTERCEPT_ACCESS_MASK_READ_WRITE: u32 =
        HV_INTERCEPT_ACCESS_MASK_READ | HV_INTERCEPT_ACCESS_MASK_WRITE;
    pub const HV_INTERCEPT_ACCESS_MASK_EXECUTE: u32 = 0x04;

    open_enum::open_enum! {
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum HvInterceptType: u32 {
            #![expect(non_upper_case_globals)]
            HvInterceptTypeX64IoPort = 0x00000000,
            HvInterceptTypeX64Msr = 0x00000001,
            HvInterceptTypeX64Cpuid = 0x00000002,
            HvInterceptTypeException = 0x00000003,
            HvInterceptTypeHypercall = 0x00000008,
            HvInterceptTypeUnknownSynicConnection = 0x0000000D,
            HvInterceptTypeX64ApicEoi = 0x0000000E,
            HvInterceptTypeRetargetInterruptWithUnknownDeviceId = 0x0000000F,
            HvInterceptTypeX64IoPortRange = 0x00000011,
        }
    }

    #[repr(transparent)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    pub struct HvInterceptParameters(u64);

    impl HvInterceptParameters {
        pub fn new_io_port(port: u16) -> Self {
            Self(port as u64)
        }

        pub fn new_io_port_range(ports: RangeInclusive<u16>) -> Self {
            let base = *ports.start() as u64;
            let end = *ports.end() as u64;
            Self(base | (end << 16))
        }

        pub fn new_exception(vector: u16) -> Self {
            Self(vector as u64)
        }

        pub fn io_port(&self) -> u16 {
            self.0 as u16
        }

        pub fn io_port_range(&self) -> RangeInclusive<u16> {
            let base = self.0 as u16;
            let end = (self.0 >> 16) as u16;
            base..=end
        }

        pub fn cpuid_index(&self) -> u32 {
            self.0 as u32
        }

        pub fn exception(&self) -> u16 {
            self.0 as u16
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    pub struct InstallIntercept {
        pub partition_id: u64,
        pub access_type_mask: u32,
        pub intercept_type: HvInterceptType,
        pub intercept_parameters: HvInterceptParameters,
    }

    #[repr(C)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    pub struct AssertVirtualInterrupt {
        pub partition_id: u64,
        pub interrupt_control: HvInterruptControl,
        pub destination_address: u64,
        pub requested_vector: u32,
        pub target_vtl: u8,
        pub rsvd0: u8,
        pub rsvd1: u16,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct StartVirtualProcessorX64 {
        pub partition_id: u64,
        pub vp_index: u32,
        pub target_vtl: u8,
        pub rsvd0: u8,
        pub rsvd1: u16,
        pub vp_context: InitialVpContextX64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct InitialVpContextX64 {
        pub rip: u64,
        pub rsp: u64,
        pub rflags: u64,
        pub cs: HvX64SegmentRegister,
        pub ds: HvX64SegmentRegister,
        pub es: HvX64SegmentRegister,
        pub fs: HvX64SegmentRegister,
        pub gs: HvX64SegmentRegister,
        pub ss: HvX64SegmentRegister,
        pub tr: HvX64SegmentRegister,
        pub ldtr: HvX64SegmentRegister,
        pub idtr: HvX64TableRegister,
        pub gdtr: HvX64TableRegister,
        pub efer: u64,
        pub cr0: u64,
        pub cr3: u64,
        pub cr4: u64,
        pub msr_cr_pat: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct StartVirtualProcessorArm64 {
        pub partition_id: u64,
        pub vp_index: u32,
        pub target_vtl: u8,
        pub rsvd0: u8,
        pub rsvd1: u16,
        pub vp_context: InitialVpContextArm64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct InitialVpContextArm64 {
        pub pc: u64,
        pub sp_elh: u64,
        pub sctlr_el1: u64,
        pub mair_el1: u64,
        pub tcr_el1: u64,
        pub vbar_el1: u64,
        pub ttbr0_el1: u64,
        pub ttbr1_el1: u64,
        pub x18: u64,
    }

    impl InitialVpContextX64 {
        pub fn as_hv_register_assocs(&self) -> impl Iterator<Item = HvRegisterAssoc> + '_ {
            let regs = [
                (HvX64RegisterName::Rip, HvRegisterValue::from(self.rip)).into(),
                (HvX64RegisterName::Rsp, HvRegisterValue::from(self.rsp)).into(),
                (
                    HvX64RegisterName::Rflags,
                    HvRegisterValue::from(self.rflags),
                )
                    .into(),
                (HvX64RegisterName::Cs, HvRegisterValue::from(self.cs)).into(),
                (HvX64RegisterName::Ds, HvRegisterValue::from(self.ds)).into(),
                (HvX64RegisterName::Es, HvRegisterValue::from(self.es)).into(),
                (HvX64RegisterName::Fs, HvRegisterValue::from(self.fs)).into(),
                (HvX64RegisterName::Gs, HvRegisterValue::from(self.gs)).into(),
                (HvX64RegisterName::Ss, HvRegisterValue::from(self.ss)).into(),
                (HvX64RegisterName::Tr, HvRegisterValue::from(self.tr)).into(),
                (HvX64RegisterName::Ldtr, HvRegisterValue::from(self.ldtr)).into(),
                (HvX64RegisterName::Idtr, HvRegisterValue::from(self.idtr)).into(),
                (HvX64RegisterName::Gdtr, HvRegisterValue::from(self.gdtr)).into(),
                (HvX64RegisterName::Efer, HvRegisterValue::from(self.efer)).into(),
                (HvX64RegisterName::Cr0, HvRegisterValue::from(self.cr0)).into(),
                (HvX64RegisterName::Cr3, HvRegisterValue::from(self.cr3)).into(),
                (HvX64RegisterName::Cr4, HvRegisterValue::from(self.cr4)).into(),
                (
                    HvX64RegisterName::Pat,
                    HvRegisterValue::from(self.msr_cr_pat),
                )
                    .into(),
            ];
            regs.into_iter()
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateGvaControlFlagsX64 {
        /// Request data read access
        pub validate_read: bool,
        /// Request data write access
        pub validate_write: bool,
        /// Request instruction fetch access.
        pub validate_execute: bool,
        /// Don't enforce any checks related to access mode (supervisor vs. user; SMEP and SMAP are treated
        /// as disabled).
        pub privilege_exempt: bool,
        /// Set the appropriate page table bits (i.e. access/dirty bit)
        pub set_page_table_bits: bool,
        /// Lock the TLB
        pub tlb_flush_inhibit: bool,
        /// Treat the access as a supervisor mode access irrespective of current mode.
        pub supervisor_access: bool,
        /// Treat the access as a user mode access irrespective of current mode.
        pub user_access: bool,
        /// Enforce the SMAP restriction on supervisor data access to user mode addresses if CR4.SMAP=1
        /// irrespective of current EFLAGS.AC i.e. the behavior for "implicit supervisor-mode accesses"
        /// (e.g. to the GDT, etc.) and when EFLAGS.AC=0. Does nothing if CR4.SMAP=0.
        pub enforce_smap: bool,
        /// Don't enforce the SMAP restriction on supervisor data access to user mode addresses irrespective
        /// of current EFLAGS.AC i.e. the behavior when EFLAGS.AC=1.
        pub override_smap: bool,
        /// Treat the access as a shadow stack access.
        pub shadow_stack: bool,
        #[bits(45)]
        _unused: u64,
        /// Target vtl
        input_vtl_value: u8,
    }

    impl TranslateGvaControlFlagsX64 {
        pub fn input_vtl(&self) -> HvInputVtl {
            self.input_vtl_value().into()
        }

        pub fn with_input_vtl(self, input_vtl: HvInputVtl) -> Self {
            self.with_input_vtl_value(input_vtl.into())
        }

        pub fn set_input_vtl(&mut self, input_vtl: HvInputVtl) {
            self.set_input_vtl_value(input_vtl.into())
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateGvaControlFlagsArm64 {
        /// Request data read access
        pub validate_read: bool,
        /// Request data write access
        pub validate_write: bool,
        /// Request instruction fetch access.
        pub validate_execute: bool,
        _reserved0: bool,
        /// Set the appropriate page table bits (i.e. access/dirty bit)
        pub set_page_table_bits: bool,
        /// Lock the TLB
        pub tlb_flush_inhibit: bool,
        /// Treat the access as a supervisor mode access irrespective of current mode.
        pub supervisor_access: bool,
        /// Treat the access as a user mode access irrespective of current mode.
        pub user_access: bool,
        /// Restrict supervisor data access to user mode addresses irrespective of current PSTATE.PAN i.e.
        /// the behavior when PSTATE.PAN=1.
        pub pan_set: bool,
        /// Don't restrict supervisor data access to user mode addresses irrespective of current PSTATE.PAN
        /// i.e. the behavior when PSTATE.PAN=0.
        pub pan_clear: bool,
        #[bits(46)]
        _unused: u64,
        /// Target vtl
        #[bits(8)]
        input_vtl_value: u8,
    }

    impl TranslateGvaControlFlagsArm64 {
        pub fn input_vtl(&self) -> HvInputVtl {
            self.input_vtl_value().into()
        }

        pub fn with_input_vtl(self, input_vtl: HvInputVtl) -> Self {
            self.with_input_vtl_value(input_vtl.into())
        }

        pub fn set_input_vtl(&mut self, input_vtl: HvInputVtl) {
            self.set_input_vtl_value(input_vtl.into())
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateVirtualAddressX64 {
        pub partition_id: u64,
        pub vp_index: u32,
        // NOTE: This reserved field is not in the OS headers, but is required due to alignment. Confirmed via debugger.
        pub reserved: u32,
        pub control_flags: TranslateGvaControlFlagsX64,
        pub gva_page: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateVirtualAddressArm64 {
        pub partition_id: u64,
        pub vp_index: u32,
        // NOTE: This reserved field is not in the OS headers, but is required due to alignment. Confirmed via debugger.
        pub reserved: u32,
        pub control_flags: TranslateGvaControlFlagsArm64,
        pub gva_page: u64,
    }

    open_enum::open_enum! {
        pub enum TranslateGvaResultCode: u32 {
            SUCCESS = 0,

            // Translation Failures
            PAGE_NOT_PRESENT = 1,
            PRIVILEGE_VIOLATION = 2,
            INVALID_PAGE_TABLE_FLAGS = 3,

            // GPA access failures
            GPA_UNMAPPED = 4,
            GPA_NO_READ_ACCESS = 5,
            GPA_NO_WRITE_ACCESS = 6,
            GPA_ILLEGAL_OVERLAY_ACCESS = 7,

            /// Intercept of the memory access by either
            /// - a higher VTL
            /// - a nested hypervisor (due to a violation of the nested page table)
            INTERCEPT = 8,

            GPA_UNACCEPTED = 9,
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateGvaResult {
        pub result_code: u32,
        pub cache_type: u8,
        pub overlay_page: bool,
        #[bits(23)]
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateVirtualAddressOutput {
        pub translation_result: TranslateGvaResult,
        pub gpa_page: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateGvaResultExX64 {
        pub result: TranslateGvaResult,
        pub reserved: u64,
        pub event_info: HvX64PendingEvent,
    }

    const_assert!(size_of::<TranslateGvaResultExX64>() == 0x30);

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateGvaResultExArm64 {
        pub result: TranslateGvaResult,
    }

    const_assert!(size_of::<TranslateGvaResultExArm64>() == 0x8);

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateVirtualAddressExOutputX64 {
        pub translation_result: TranslateGvaResultExX64,
        pub gpa_page: u64,
        // NOTE: This reserved field is not in the OS headers, but is required due to alignment. Confirmed via debugger.
        pub reserved: u64,
    }

    const_assert!(size_of::<TranslateVirtualAddressExOutputX64>() == 0x40);

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TranslateVirtualAddressExOutputArm64 {
        pub translation_result: TranslateGvaResultExArm64,
        pub gpa_page: u64,
    }

    const_assert!(size_of::<TranslateVirtualAddressExOutputArm64>() == 0x10);

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct GetVpIndexFromApicId {
        pub partition_id: u64,
        pub target_vtl: u8,
        pub reserved: [u8; 7],
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct EnableVpVtlX64 {
        pub partition_id: u64,
        pub vp_index: u32,
        pub target_vtl: u8,
        pub reserved: [u8; 3],
        pub vp_vtl_context: InitialVpContextX64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct EnableVpVtlArm64 {
        pub partition_id: u64,
        pub vp_index: u32,
        pub target_vtl: u8,
        pub reserved: [u8; 3],
        pub vp_vtl_context: InitialVpContextArm64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ModifyVtlProtectionMask {
        pub partition_id: u64,
        pub map_flags: HvMapGpaFlags,
        pub target_vtl: HvInputVtl,
        pub reserved: [u8; 3],
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct CheckSparseGpaPageVtlAccess {
        pub partition_id: u64,
        pub target_vtl: HvInputVtl,
        pub desired_access: u8,
        pub reserved0: u16,
        pub reserved1: u32,
    }
    const_assert!(size_of::<CheckSparseGpaPageVtlAccess>() == 0x10);

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct CheckSparseGpaPageVtlAccessOutput {
        pub result_code: u8,
        pub denied_access: u8,
        #[bits(4)]
        pub intercepting_vtl: u32,
        #[bits(12)]
        _reserved0: u32,
        _reserved1: u32,
    }
    const_assert!(size_of::<CheckSparseGpaPageVtlAccessOutput>() == 0x8);

    open_enum::open_enum! {
        pub enum CheckGpaPageVtlAccessResultCode: u32 {
            SUCCESS = 0,
            MEMORY_INTERCEPT = 1,
        }
    }

    /// The number of VTLs for which permissions can be specified in a VTL permission set.
    pub const HV_VTL_PERMISSION_SET_SIZE: usize = 2;

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VtlPermissionSet {
        /// VTL permissions for the GPA page, starting from VTL 1.
        pub vtl_permission_from_1: [u16; HV_VTL_PERMISSION_SET_SIZE],
    }

    open_enum::open_enum! {
        pub enum AcceptMemoryType: u32 {
            ANY = 0,
            RAM = 1,
        }
    }

    open_enum! {
        /// Host visibility used in hypercall inputs.
        ///
        /// NOTE: While this is a 2 bit set with the lower bit representing host
        /// read access and upper bit representing host write access, hardware
        /// platforms do not support that form of isolation. Only support
        /// private or full shared in this definition.
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum HostVisibilityType: u8 {
            PRIVATE = 0,
            SHARED = 3,
        }
    }

    // Used by bitfield-struct implicitly.
    impl HostVisibilityType {
        const fn from_bits(value: u8) -> Self {
            Self(value)
        }

        const fn into_bits(value: Self) -> u8 {
            value.0
        }
    }

    /// Attributes for accepting pages. See [`AcceptGpaPages`]
    #[bitfield(u32)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct AcceptPagesAttributes {
        #[bits(6)]
        /// Supplies the expected memory type [`AcceptMemoryType`].
        pub memory_type: u32,
        #[bits(2)]
        /// Supplies the initial host visibility (exclusive, shared read-only, shared read-write).
        pub host_visibility: HostVisibilityType,
        #[bits(3)]
        /// Supplies the set of VTLs for which initial VTL permissions will be set.
        pub vtl_set: u32,
        #[bits(21)]
        _reserved: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct AcceptGpaPages {
        /// Supplies the partition ID of the partition this request is for.
        pub partition_id: u64,
        /// Supplies attributes of the pages being accepted, such as whether
        /// they should be made host visible.
        pub page_attributes: AcceptPagesAttributes,
        /// Supplies the set of initial VTL permissions.
        pub vtl_permission_set: VtlPermissionSet,
        /// Supplies the GPA page number of the first page to modify.
        pub gpa_page_base: u64,
    }
    const_assert!(size_of::<AcceptGpaPages>() == 0x18);

    /// Attributes for unaccepting pages. See [`UnacceptGpaPages`]
    #[bitfield(u32)]
    pub struct UnacceptPagesAttributes {
        #[bits(3)]
        pub vtl_set: u32,
        #[bits(29)]
        _reserved: u32,
    }

    #[repr(C)]
    pub struct UnacceptGpaPages {
        /// Supplies the partition ID of the partition this request is for.
        pub partition_id: u64,
        /// Supplies the set of VTLs for which VTL permissions will be checked.
        pub page_attributes: UnacceptPagesAttributes,
        ///  Supplies the set of VTL permissions to check against.
        pub vtl_permission_set: VtlPermissionSet,
        /// Supplies the GPA page number of the first page to modify.
        pub gpa_page_base: u64,
    }
    const_assert!(size_of::<UnacceptGpaPages>() == 0x18);

    #[bitfield(u32)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ModifyHostVisibility {
        #[bits(2)]
        pub host_visibility: HostVisibilityType,
        #[bits(30)]
        _reserved: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ModifySparsePageVisibility {
        pub partition_id: u64,
        pub host_visibility: ModifyHostVisibility,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct QuerySparsePageVisibility {
        pub partition_id: u64,
    }

    pub const VBS_VM_REPORT_DATA_SIZE: usize = 64;
    pub const VBS_VM_MAX_REPORT_SIZE: usize = 2048;

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VbsVmCallReport {
        pub report_data: [u8; VBS_VM_REPORT_DATA_SIZE],
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VbsVmCallReportOutput {
        pub report: [u8; VBS_VM_MAX_REPORT_SIZE],
    }

    #[bitfield(u8)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct EnablePartitionVtlFlags {
        pub enable_mbec: bool,
        pub enable_supervisor_shadow_stack: bool,
        pub enable_hardware_hvpt: bool,
        #[bits(5)]
        pub reserved: u8,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct EnablePartitionVtl {
        pub partition_id: u64,
        pub target_vtl: u8,
        pub flags: EnablePartitionVtlFlags,
        pub reserved_z0: u16,
        pub reserved_z1: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct FlushVirtualAddressSpace {
        pub address_space: u64,
        pub flags: HvFlushFlags,
        pub processor_mask: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct FlushVirtualAddressSpaceEx {
        pub address_space: u64,
        pub flags: HvFlushFlags,
        pub vp_set_format: u64,
        pub vp_set_valid_banks_mask: u64,
        // Followed by the variable-sized part of an HvVpSet
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PinUnpinGpaPageRangesHeader {
        pub reserved: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SendSyntheticClusterIpi {
        pub vector: u32,
        pub target_vtl: HvInputVtl,
        pub flags: u8,
        pub reserved: u16,
        pub processor_mask: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SendSyntheticClusterIpiEx {
        pub vector: u32,
        pub target_vtl: HvInputVtl,
        pub flags: u8,
        pub reserved: u16,
        pub vp_set_format: u64,
        pub vp_set_valid_banks_mask: u64,
        // Followed by the variable-sized part of an HvVpSet
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvFlushFlags {
        pub all_processors: bool,
        pub all_virtual_address_spaces: bool,
        pub non_global_mappings_only: bool,
        pub use_extended_range_format: bool,
        pub use_target_vtl: bool,

        #[bits(3)]
        _reserved: u8,

        pub target_vtl0: bool,
        pub target_vtl1: bool,

        #[bits(54)]
        _reserved2: u64,
    }

    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(transparent)]
    pub struct HvGvaRange(pub u64);

    impl From<u64> for HvGvaRange {
        fn from(value: u64) -> Self {
            Self(value)
        }
    }

    impl From<HvGvaRange> for u64 {
        fn from(value: HvGvaRange) -> Self {
            value.0
        }
    }

    impl HvGvaRange {
        pub fn as_simple(self) -> HvGvaRangeSimple {
            HvGvaRangeSimple(self.0)
        }

        pub fn as_extended(self) -> HvGvaRangeExtended {
            HvGvaRangeExtended(self.0)
        }

        pub fn as_extended_large_page(self) -> HvGvaRangeExtendedLargePage {
            HvGvaRangeExtendedLargePage(self.0)
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGvaRangeSimple {
        /// The number of pages beyond one.
        #[bits(12)]
        pub additional_pages: u64,
        /// The top 52 most significant bits of the guest virtual address.
        #[bits(52)]
        pub gva_page_number: u64,
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGvaRangeExtended {
        /// The number of pages beyond one.
        #[bits(11)]
        pub additional_pages: u64,
        /// Is page size greater than 4 KB.
        pub large_page: bool,
        /// The top 52 most significant bits of the guest virtual address when `large_page`` is clear.
        #[bits(52)]
        pub gva_page_number: u64,
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGvaRangeExtendedLargePage {
        /// The number of pages beyond one.
        #[bits(11)]
        pub additional_pages: u64,
        /// Is page size greater than 4 KB.
        pub large_page: bool,
        /// The page size when `large_page`` is set.
        /// false: 2 MB
        /// true: 1 GB
        pub page_size: bool,
        #[bits(8)]
        _reserved: u64,
        /// The top 43 most significant bits of the guest virtual address when `large_page`` is set.
        #[bits(43)]
        pub gva_large_page_number: u64,
    }

    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(transparent)]
    pub struct HvGpaRange(pub u64);

    impl HvGpaRange {
        pub fn as_simple(self) -> HvGpaRangeSimple {
            HvGpaRangeSimple(self.0)
        }

        pub fn as_extended(self) -> HvGpaRangeExtended {
            HvGpaRangeExtended(self.0)
        }

        pub fn as_extended_large_page(self) -> HvGpaRangeExtendedLargePage {
            HvGpaRangeExtendedLargePage(self.0)
        }
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGpaRangeSimple {
        /// The number of pages beyond one.
        #[bits(12)]
        pub additional_pages: u64,
        /// The top 52 most significant bits of the guest physical address.
        #[bits(52)]
        pub gpa_page_number: u64,
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGpaRangeExtended {
        /// The number of pages beyond one.
        #[bits(11)]
        pub additional_pages: u64,
        /// Is page size greater than 4 KB.
        pub large_page: bool,
        /// The top 52 most significant bits of the guest physical address when `large_page`` is clear.
        #[bits(52)]
        pub gpa_page_number: u64,
    }

    #[bitfield(u64)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct HvGpaRangeExtendedLargePage {
        /// The number of pages beyond one.
        #[bits(11)]
        pub additional_pages: u64,
        /// Is page size greater than 4 KB.
        pub large_page: bool,
        /// The page size when `large_page`` is set.
        /// false: 2 MB
        /// true: 1 GB
        pub page_size: bool,
        #[bits(8)]
        _reserved: u64,
        /// The top 43 most significant bits of the guest physical address when `large_page`` is set.
        #[bits(43)]
        pub gpa_large_page_number: u64,
    }

    pub const HV_HYPERCALL_MMIO_MAX_DATA_LENGTH: usize = 64;

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct MemoryMappedIoRead {
        pub gpa: u64,
        pub access_width: u32,
        pub reserved_z0: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct MemoryMappedIoReadOutput {
        pub data: [u8; HV_HYPERCALL_MMIO_MAX_DATA_LENGTH],
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct MemoryMappedIoWrite {
        pub gpa: u64,
        pub access_width: u32,
        pub reserved_z0: u32,
        pub data: [u8; HV_HYPERCALL_MMIO_MAX_DATA_LENGTH],
    }
}

macro_rules! registers {
    ($name:ident {
        $(
            $(#[$vattr:meta])*
            $variant:ident = $value:expr
        ),*
        $(,)?
    }) => {
        open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
            pub enum $name: u32 {
        #![expect(non_upper_case_globals)]
                $($variant = $value,)*
                InstructionEmulationHints = 0x00000002,
                InternalActivityState = 0x00000004,

        // Guest Crash Registers
                GuestCrashP0  = 0x00000210,
                GuestCrashP1  = 0x00000211,
                GuestCrashP2  = 0x00000212,
                GuestCrashP3  = 0x00000213,
                GuestCrashP4  = 0x00000214,
                GuestCrashCtl = 0x00000215,

                PendingInterruption = 0x00010002,
                InterruptState = 0x00010003,
                PendingEvent0 = 0x00010004,
                PendingEvent1 = 0x00010005,
                DeliverabilityNotifications = 0x00010006,

                GicrBaseGpa = 0x00063000,

                VpRuntime = 0x00090000,
                GuestOsId = 0x00090002,
                VpIndex = 0x00090003,
                TimeRefCount = 0x00090004,
                CpuManagementVersion = 0x00090007,
                VpAssistPage = 0x00090013,
                VpRootSignalCount = 0x00090014,
                ReferenceTsc = 0x00090017,
                VpConfig = 0x00090018,
                Ghcb = 0x00090019,
                ReferenceTscSequence = 0x0009001A,
                GuestSchedulerEvent = 0x0009001B,

                Sint0 = 0x000A0000,
                Sint1 = 0x000A0001,
                Sint2 = 0x000A0002,
                Sint3 = 0x000A0003,
                Sint4 = 0x000A0004,
                Sint5 = 0x000A0005,
                Sint6 = 0x000A0006,
                Sint7 = 0x000A0007,
                Sint8 = 0x000A0008,
                Sint9 = 0x000A0009,
                Sint10 = 0x000A000A,
                Sint11 = 0x000A000B,
                Sint12 = 0x000A000C,
                Sint13 = 0x000A000D,
                Sint14 = 0x000A000E,
                Sint15 = 0x000A000F,
                Scontrol = 0x000A0010,
                Sversion = 0x000A0011,
                Sifp = 0x000A0012,
                Sipp = 0x000A0013,
                Eom = 0x000A0014,
                Sirbp = 0x000A0015,

                Stimer0Config = 0x000B0000,
                Stimer0Count = 0x000B0001,
                Stimer1Config = 0x000B0002,
                Stimer1Count = 0x000B0003,
                Stimer2Config = 0x000B0004,
                Stimer2Count = 0x000B0005,
                Stimer3Config = 0x000B0006,
                Stimer3Count = 0x000B0007,
                StimeUnhaltedTimerConfig = 0x000B0100,
                StimeUnhaltedTimerCount = 0x000B0101,

                VsmCodePageOffsets = 0x000D0002,
                VsmVpStatus = 0x000D0003,
                VsmPartitionStatus = 0x000D0004,
                VsmVina = 0x000D0005,
                VsmCapabilities = 0x000D0006,
                VsmPartitionConfig = 0x000D0007,
                GuestVsmPartitionConfig = 0x000D0008,
                VsmVpSecureConfigVtl0 = 0x000D0010,
                VsmVpSecureConfigVtl1 = 0x000D0011,
                VsmVpSecureConfigVtl2 = 0x000D0012,
                VsmVpSecureConfigVtl3 = 0x000D0013,
                VsmVpSecureConfigVtl4 = 0x000D0014,
                VsmVpSecureConfigVtl5 = 0x000D0015,
                VsmVpSecureConfigVtl6 = 0x000D0016,
                VsmVpSecureConfigVtl7 = 0x000D0017,
                VsmVpSecureConfigVtl8 = 0x000D0018,
                VsmVpSecureConfigVtl9 = 0x000D0019,
                VsmVpSecureConfigVtl10 = 0x000D001A,
                VsmVpSecureConfigVtl11 = 0x000D001B,
                VsmVpSecureConfigVtl12 = 0x000D001C,
                VsmVpSecureConfigVtl13 = 0x000D001D,
                VsmVpSecureConfigVtl14 = 0x000D001E,
                VsmVpWaitForTlbLock = 0x000D0020,
            }
        }

        impl From<HvRegisterName> for $name {
            fn from(name: HvRegisterName) -> Self {
                Self(name.0)
            }
        }

        impl From<$name> for HvRegisterName {
            fn from(name: $name) -> Self {
                Self(name.0)
            }
        }
    };
}

/// A hypervisor register for any architecture.
///
/// This exists only to pass registers through layers where the architecture
/// type has been lost. In general, you should use the arch-specific registers.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvRegisterName(pub u32);

registers! {
    // Typed enum for registers that are shared across architectures.
    HvAllArchRegisterName {}
}

impl From<HvAllArchRegisterName> for HvX64RegisterName {
    fn from(name: HvAllArchRegisterName) -> Self {
        Self(name.0)
    }
}

impl From<HvAllArchRegisterName> for HvArm64RegisterName {
    fn from(name: HvAllArchRegisterName) -> Self {
        Self(name.0)
    }
}

registers! {
    HvX64RegisterName {
        // X64 User-Mode Registers
        Rax = 0x00020000,
        Rcx = 0x00020001,
        Rdx = 0x00020002,
        Rbx = 0x00020003,
        Rsp = 0x00020004,
        Rbp = 0x00020005,
        Rsi = 0x00020006,
        Rdi = 0x00020007,
        R8 = 0x00020008,
        R9 = 0x00020009,
        R10 = 0x0002000a,
        R11 = 0x0002000b,
        R12 = 0x0002000c,
        R13 = 0x0002000d,
        R14 = 0x0002000e,
        R15 = 0x0002000f,
        Rip = 0x00020010,
        Rflags = 0x00020011,

        // X64 Floating Point and Vector Registers
        Xmm0 = 0x00030000,
        Xmm1 = 0x00030001,
        Xmm2 = 0x00030002,
        Xmm3 = 0x00030003,
        Xmm4 = 0x00030004,
        Xmm5 = 0x00030005,
        Xmm6 = 0x00030006,
        Xmm7 = 0x00030007,
        Xmm8 = 0x00030008,
        Xmm9 = 0x00030009,
        Xmm10 = 0x0003000A,
        Xmm11 = 0x0003000B,
        Xmm12 = 0x0003000C,
        Xmm13 = 0x0003000D,
        Xmm14 = 0x0003000E,
        Xmm15 = 0x0003000F,
        FpMmx0 = 0x00030010,
        FpMmx1 = 0x00030011,
        FpMmx2 = 0x00030012,
        FpMmx3 = 0x00030013,
        FpMmx4 = 0x00030014,
        FpMmx5 = 0x00030015,
        FpMmx6 = 0x00030016,
        FpMmx7 = 0x00030017,
        FpControlStatus = 0x00030018,
        XmmControlStatus = 0x00030019,

        // X64 Control Registers
        Cr0 = 0x00040000,
        Cr2 = 0x00040001,
        Cr3 = 0x00040002,
        Cr4 = 0x00040003,
        Cr8 = 0x00040004,
        Xfem = 0x00040005,
        // X64 Intermediate Control Registers
        IntermediateCr0 = 0x00041000,
        IntermediateCr3 = 0x00041002,
        IntermediateCr4 = 0x00041003,
        IntermediateCr8 = 0x00041004,
        // X64 Debug Registers
        Dr0 = 0x00050000,
        Dr1 = 0x00050001,
        Dr2 = 0x00050002,
        Dr3 = 0x00050003,
        Dr6 = 0x00050004,
        Dr7 = 0x00050005,
        // X64 Segment Registers
        Es = 0x00060000,
        Cs = 0x00060001,
        Ss = 0x00060002,
        Ds = 0x00060003,
        Fs = 0x00060004,
        Gs = 0x00060005,
        Ldtr = 0x00060006,
        Tr = 0x00060007,
        // X64 Table Registers
        Idtr = 0x00070000,
        Gdtr = 0x00070001,
        // X64 Virtualized MSRs
        Tsc = 0x00080000,
        Efer = 0x00080001,
        KernelGsBase = 0x00080002,
        ApicBase = 0x00080003,
        Pat = 0x00080004,
        SysenterCs = 0x00080005,
        SysenterEip = 0x00080006,
        SysenterEsp = 0x00080007,
        Star = 0x00080008,
        Lstar = 0x00080009,
        Cstar = 0x0008000a,
        Sfmask = 0x0008000b,
        InitialApicId = 0x0008000c,
        // X64 Cache control MSRs
        MsrMtrrCap = 0x0008000d,
        MsrMtrrDefType = 0x0008000e,
        MsrMtrrPhysBase0 = 0x00080010,
        MsrMtrrPhysBase1 = 0x00080011,
        MsrMtrrPhysBase2 = 0x00080012,
        MsrMtrrPhysBase3 = 0x00080013,
        MsrMtrrPhysBase4 = 0x00080014,
        MsrMtrrPhysBase5 = 0x00080015,
        MsrMtrrPhysBase6 = 0x00080016,
        MsrMtrrPhysBase7 = 0x00080017,
        MsrMtrrPhysBase8 = 0x00080018,
        MsrMtrrPhysBase9 = 0x00080019,
        MsrMtrrPhysBaseA = 0x0008001a,
        MsrMtrrPhysBaseB = 0x0008001b,
        MsrMtrrPhysBaseC = 0x0008001c,
        MsrMtrrPhysBaseD = 0x0008001d,
        MsrMtrrPhysBaseE = 0x0008001e,
        MsrMtrrPhysBaseF = 0x0008001f,
        MsrMtrrPhysMask0 = 0x00080040,
        MsrMtrrPhysMask1 = 0x00080041,
        MsrMtrrPhysMask2 = 0x00080042,
        MsrMtrrPhysMask3 = 0x00080043,
        MsrMtrrPhysMask4 = 0x00080044,
        MsrMtrrPhysMask5 = 0x00080045,
        MsrMtrrPhysMask6 = 0x00080046,
        MsrMtrrPhysMask7 = 0x00080047,
        MsrMtrrPhysMask8 = 0x00080048,
        MsrMtrrPhysMask9 = 0x00080049,
        MsrMtrrPhysMaskA = 0x0008004a,
        MsrMtrrPhysMaskB = 0x0008004b,
        MsrMtrrPhysMaskC = 0x0008004c,
        MsrMtrrPhysMaskD = 0x0008004d,
        MsrMtrrPhysMaskE = 0x0008004e,
        MsrMtrrPhysMaskF = 0x0008004f,
        MsrMtrrFix64k00000 = 0x00080070,
        MsrMtrrFix16k80000 = 0x00080071,
        MsrMtrrFix16kA0000 = 0x00080072,
        MsrMtrrFix4kC0000 = 0x00080073,
        MsrMtrrFix4kC8000 = 0x00080074,
        MsrMtrrFix4kD0000 = 0x00080075,
        MsrMtrrFix4kD8000 = 0x00080076,
        MsrMtrrFix4kE0000 = 0x00080077,
        MsrMtrrFix4kE8000 = 0x00080078,
        MsrMtrrFix4kF0000 = 0x00080079,
        MsrMtrrFix4kF8000 = 0x0008007a,

        TscAux = 0x0008007B,
        Bndcfgs = 0x0008007C,
        DebugCtl = 0x0008007D,
        MCount = 0x0008007E,
        ACount = 0x0008007F,

        SgxLaunchControl0 = 0x00080080,
        SgxLaunchControl1 = 0x00080081,
        SgxLaunchControl2 = 0x00080082,
        SgxLaunchControl3 = 0x00080083,
        SpecCtrl = 0x00080084,
        PredCmd = 0x00080085,
        VirtSpecCtrl = 0x00080086,
        TscVirtualOffset = 0x00080087,
        TsxCtrl = 0x00080088,
        MsrMcUpdatePatchLevel = 0x00080089,
        Available1 = 0x0008008A,
        Xss = 0x0008008B,
        UCet = 0x0008008C,
        SCet = 0x0008008D,
        Ssp = 0x0008008E,
        Pl0Ssp = 0x0008008F,
        Pl1Ssp = 0x00080090,
        Pl2Ssp = 0x00080091,
        Pl3Ssp = 0x00080092,
        InterruptSspTableAddr = 0x00080093,
        TscVirtualMultiplier = 0x00080094,
        TscDeadline = 0x00080095,
        TscAdjust = 0x00080096,
        Pasid = 0x00080097,
        UmwaitControl = 0x00080098,
        Xfd = 0x00080099,
        XfdErr = 0x0008009A,

        Hypercall = 0x00090001,
        RegisterPage = 0x0009001C,

        // Partition Timer Assist Registers
        EmulatedTimerPeriod = 0x00090030,
        EmulatedTimerControl = 0x00090031,
        PmTimerAssist = 0x00090032,

        // AMD SEV configuration MSRs
        SevControl = 0x00090040,

        CrInterceptControl = 0x000E0000,
        CrInterceptCr0Mask = 0x000E0001,
        CrInterceptCr4Mask = 0x000E0002,
        CrInterceptIa32MiscEnableMask = 0x000E0003,
    }
}

registers! {
    HvArm64RegisterName {
        HypervisorVersion = 0x00000100,
        PrivilegesAndFeaturesInfo = 0x00000200,
        FeaturesInfo = 0x00000201,
        ImplementationLimitsInfo = 0x00000202,
        HardwareFeaturesInfo = 0x00000203,
        CpuManagementFeaturesInfo = 0x00000204,
        PasidFeaturesInfo = 0x00000205,
        SkipLevelFeaturesInfo = 0x00000206,
        NestedVirtFeaturesInfo = 0x00000207,
        IptFeaturesInfo = 0x00000208,
        IsolationConfiguration = 0x00000209,

        X0 = 0x00020000,
        X1 = 0x00020001,
        X2 = 0x00020002,
        X3 = 0x00020003,
        X4 = 0x00020004,
        X5 = 0x00020005,
        X6 = 0x00020006,
        X7 = 0x00020007,
        X8 = 0x00020008,
        X9 = 0x00020009,
        X10 = 0x0002000A,
        X11 = 0x0002000B,
        X12 = 0x0002000C,
        X13 = 0x0002000D,
        X14 = 0x0002000E,
        X15 = 0x0002000F,
        X16 = 0x00020010,
        X17 = 0x00020011,
        X18 = 0x00020012,
        X19 = 0x00020013,
        X20 = 0x00020014,
        X21 = 0x00020015,
        X22 = 0x00020016,
        X23 = 0x00020017,
        X24 = 0x00020018,
        X25 = 0x00020019,
        X26 = 0x0002001A,
        X27 = 0x0002001B,
        X28 = 0x0002001C,
        XFp = 0x0002001D,
        XLr = 0x0002001E,
        XSp = 0x0002001F, // alias for either El0/x depending on Cpsr.SPSel
        XSpEl0 = 0x00020020,
        XSpElx = 0x00020021,
        XPc = 0x00020022,
        Cpsr = 0x00020023,
        SpsrEl2 = 0x00021002,

        SctlrEl1 = 0x00040002,
        Ttbr0El1 = 0x00040005,
        Ttbr1El1 = 0x00040006,
        TcrEl1 = 0x00040007,
        EsrEl1 = 0x00040008,
        FarEl1 = 0x00040009,
        MairEl1 = 0x0004000b,
        VbarEl1 = 0x0004000c,
        ElrEl1 = 0x00040015,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvRegisterValue(pub AlignedU128);

impl HvRegisterValue {
    pub fn as_u128(&self) -> u128 {
        self.0.into()
    }

    pub fn as_u64(&self) -> u64 {
        self.as_u128() as u64
    }

    pub fn as_u32(&self) -> u32 {
        self.as_u128() as u32
    }

    pub fn as_u16(&self) -> u16 {
        self.as_u128() as u16
    }

    pub fn as_u8(&self) -> u8 {
        self.as_u128() as u8
    }

    pub fn as_table(&self) -> HvX64TableRegister {
        HvX64TableRegister::read_from_prefix(self.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    pub fn as_segment(&self) -> HvX64SegmentRegister {
        HvX64SegmentRegister::read_from_prefix(self.as_bytes())
            .unwrap()
            .0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

impl From<u8> for HvRegisterValue {
    fn from(val: u8) -> Self {
        (val as u128).into()
    }
}

impl From<u16> for HvRegisterValue {
    fn from(val: u16) -> Self {
        (val as u128).into()
    }
}

impl From<u32> for HvRegisterValue {
    fn from(val: u32) -> Self {
        (val as u128).into()
    }
}

impl From<u64> for HvRegisterValue {
    fn from(val: u64) -> Self {
        (val as u128).into()
    }
}

impl From<u128> for HvRegisterValue {
    fn from(val: u128) -> Self {
        Self(val.into())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64TableRegister {
    pub pad: [u16; 3],
    pub limit: u16,
    pub base: u64,
}

impl From<HvX64TableRegister> for HvRegisterValue {
    fn from(val: HvX64TableRegister) -> Self {
        Self::read_from_prefix(val.as_bytes()).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

impl From<HvRegisterValue> for HvX64TableRegister {
    fn from(val: HvRegisterValue) -> Self {
        Self::read_from_prefix(val.as_bytes()).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64SegmentRegister {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attributes: u16,
}

impl From<HvX64SegmentRegister> for HvRegisterValue {
    fn from(val: HvX64SegmentRegister) -> Self {
        Self::read_from_prefix(val.as_bytes()).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

impl From<HvRegisterValue> for HvX64SegmentRegister {
    fn from(val: HvRegisterValue) -> Self {
        Self::read_from_prefix(val.as_bytes()).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct HvDeliverabilityNotificationsRegister {
    /// x86_64 only.
    pub nmi_notification: bool,
    /// x86_64 only.
    pub interrupt_notification: bool,
    /// x86_64 only.
    #[bits(4)]
    /// Only used on x86_64.
    pub interrupt_priority: u8,
    #[bits(42)]
    pub reserved: u64,
    pub sints: u16,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvVtlEntryReason: u32 {
        /// This reason is reserved and is not used.
        RESERVED = 0,

        /// Indicates entry due to a VTL call from a lower VTL.
        VTL_CALL = 1,

        /// Indicates entry due to an interrupt targeted to the VTL.
        INTERRUPT = 2,

        // Indicates an entry due to an intercept delivered via the intercept page.
        INTERCEPT = 3,
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvVpVtlControl {
    //
    // The hypervisor updates the entry reason with an indication as to why the
    // VTL was entered on the virtual processor.
    //
    pub entry_reason: HvVtlEntryReason,

    /// This flag determines whether the VINA interrupt line is asserted.
    pub vina_status: u8,
    pub reserved_z0: u8,
    pub reserved_z1: u16,

    /// A guest updates the VtlReturn* fields to provide the register values to
    /// restore on VTL return.  The specific register values that are restored
    /// will vary based on whether the VTL is 32-bit or 64-bit: rax and rcx or
    /// eax, ecx, and edx.
    pub registers: [u64; 2],
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvRegisterVsmVina {
    pub vector: u8,
    pub enabled: bool,
    pub auto_reset: bool,
    pub auto_eoi: bool,
    #[bits(53)]
    pub reserved: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvVpAssistPage {
    /// APIC assist for optimized EOI processing.
    pub apic_assist: u32,
    pub reserved_z0: u32,

    /// VP-VTL control information
    pub vtl_control: HvVpVtlControl,

    pub nested_enlightenments_control: u64,
    pub enlighten_vm_entry: u8,
    pub reserved_z1: [u8; 7],
    pub current_nested_vmcs: u64,
    pub synthetic_time_unhalted_timer_expired: u8,
    pub reserved_z2: [u8; 7],
    pub virtualization_fault_information: [u8; 40],
    pub reserved_z3: u64,
    pub intercept_message: HvMessage,
    pub vtl_return_actions: [u8; 256],
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvVpAssistPageActionSignalEvent {
    pub action_type: u64,
    pub target_vp: u32,
    pub target_vtl: u8,
    pub target_sint: u8,
    pub flag_number: u16,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvInterceptAccessType: u8 {
        READ = 0,
        WRITE = 1,
        EXECUTE = 2,
    }
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64VpExecutionState {
    #[bits(2)]
    pub cpl: u8,
    pub cr0_pe: bool,
    pub cr0_am: bool,
    pub efer_lma: bool,
    pub debug_active: bool,
    pub interruption_pending: bool,
    #[bits(4)]
    pub vtl: u8,
    pub enclave_mode: bool,
    pub interrupt_shadow: bool,
    pub virtualization_fault_active: bool,
    #[bits(2)]
    pub reserved: u8,
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64VpExecutionState {
    #[bits(2)]
    pub cpl: u8,
    pub debug_active: bool,
    pub interruption_pending: bool,
    #[bits(4)]
    pub vtl: u8,
    pub virtualization_fault_active: bool,
    #[bits(7)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64InterceptMessageHeader {
    pub vp_index: u32,
    pub instruction_length_and_cr8: u8,
    pub intercept_access_type: HvInterceptAccessType,
    pub execution_state: HvX64VpExecutionState,
    pub cs_segment: HvX64SegmentRegister,
    pub rip: u64,
    pub rflags: u64,
}

impl MessagePayload for HvX64InterceptMessageHeader {}

impl HvX64InterceptMessageHeader {
    pub fn instruction_len(&self) -> u8 {
        self.instruction_length_and_cr8 & 0xf
    }

    pub fn cr8(&self) -> u8 {
        self.instruction_length_and_cr8 >> 4
    }
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64InterceptMessageHeader {
    pub vp_index: u32,
    pub instruction_length: u8,
    pub intercept_access_type: HvInterceptAccessType,
    pub execution_state: HvArm64VpExecutionState,
    pub pc: u64,
    pub cspr: u64,
}
const_assert!(size_of::<HvArm64InterceptMessageHeader>() == 0x18);

impl MessagePayload for HvArm64InterceptMessageHeader {}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64IoPortAccessInfo(pub u8);

impl HvX64IoPortAccessInfo {
    pub fn new(access_size: u8, string_op: bool, rep_prefix: bool) -> Self {
        let mut info = access_size & 0x7;

        if string_op {
            info |= 0x8;
        }

        if rep_prefix {
            info |= 0x10;
        }

        Self(info)
    }

    pub fn access_size(&self) -> u8 {
        self.0 & 0x7
    }

    pub fn string_op(&self) -> bool {
        self.0 & 0x8 != 0
    }

    pub fn rep_prefix(&self) -> bool {
        self.0 & 0x10 != 0
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64IoPortInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub port_number: u16,
    pub access_info: HvX64IoPortAccessInfo,
    pub instruction_byte_count: u8,
    pub reserved: u32,
    pub rax: u64,
    pub instruction_bytes: [u8; 16],
    pub ds_segment: HvX64SegmentRegister,
    pub es_segment: HvX64SegmentRegister,
    pub rcx: u64,
    pub rsi: u64,
    pub rdi: u64,
}

impl MessagePayload for HvX64IoPortInterceptMessage {}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64MemoryAccessInfo {
    pub gva_valid: bool,
    pub gva_gpa_valid: bool,
    pub hypercall_output_pending: bool,
    pub tlb_locked: bool,
    pub supervisor_shadow_stack: bool,
    #[bits(3)]
    pub reserved1: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64MemoryAccessInfo {
    pub gva_valid: bool,
    pub gva_gpa_valid: bool,
    pub hypercall_output_pending: bool,
    #[bits(5)]
    pub reserved1: u8,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvCacheType: u32 {
        #![expect(non_upper_case_globals)]
        HvCacheTypeUncached = 0,
        HvCacheTypeWriteCombining = 1,
        HvCacheTypeWriteThrough = 4,
        HvCacheTypeWriteProtected = 5,
        HvCacheTypeWriteBack = 6,
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64MemoryInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub cache_type: HvCacheType,
    pub instruction_byte_count: u8,
    pub memory_access_info: HvX64MemoryAccessInfo,
    pub tpr_priority: u8,
    pub reserved: u8,
    pub guest_virtual_address: u64,
    pub guest_physical_address: u64,
    pub instruction_bytes: [u8; 16],
}

impl MessagePayload for HvX64MemoryInterceptMessage {}
const_assert!(size_of::<HvX64MemoryInterceptMessage>() == 0x50);

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64MemoryInterceptMessage {
    pub header: HvArm64InterceptMessageHeader,
    pub cache_type: HvCacheType,
    pub instruction_byte_count: u8,
    pub memory_access_info: HvArm64MemoryAccessInfo,
    pub reserved1: u16,
    pub instruction_bytes: [u8; 4],
    pub reserved2: u32,
    pub guest_virtual_address: u64,
    pub guest_physical_address: u64,
    pub syndrome: u64,
}

impl MessagePayload for HvArm64MemoryInterceptMessage {}
const_assert!(size_of::<HvArm64MemoryInterceptMessage>() == 0x40);

#[repr(C)]
#[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct HvArm64MmioInterceptMessage {
    pub header: HvArm64InterceptMessageHeader,
    pub guest_physical_address: u64,
    pub access_size: u32,
    pub data: [u8; 32],
    pub padding: u32,
}

impl MessagePayload for HvArm64MmioInterceptMessage {}
const_assert!(size_of::<HvArm64MmioInterceptMessage>() == 0x48);

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64MsrInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub msr_number: u32,
    pub reserved: u32,
    pub rdx: u64,
    pub rax: u64,
}

impl MessagePayload for HvX64MsrInterceptMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64SipiInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub target_vp_index: u32,
    pub vector: u32,
}

impl MessagePayload for HvX64SipiInterceptMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64SynicSintDeliverableMessage {
    pub header: HvX64InterceptMessageHeader,
    pub deliverable_sints: u16,
    pub rsvd1: u16,
    pub rsvd2: u32,
}

impl MessagePayload for HvX64SynicSintDeliverableMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64SynicSintDeliverableMessage {
    pub header: HvArm64InterceptMessageHeader,
    pub deliverable_sints: u16,
    pub rsvd1: u16,
    pub rsvd2: u32,
}

impl MessagePayload for HvArm64SynicSintDeliverableMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64InterruptionDeliverableMessage {
    pub header: HvX64InterceptMessageHeader,
    pub deliverable_type: HvX64PendingInterruptionType,
    pub rsvd: [u8; 3],
    pub rsvd2: u32,
}

impl MessagePayload for HvX64InterruptionDeliverableMessage {}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvX64PendingInterruptionType: u8 {
        HV_X64_PENDING_INTERRUPT = 0,
        HV_X64_PENDING_NMI = 2,
        HV_X64_PENDING_EXCEPTION = 3,
        HV_X64_PENDING_SOFTWARE_INTERRUPT = 4,
        HV_X64_PENDING_PRIVILEGED_SOFTWARE_EXCEPTION = 5,
        HV_X64_PENDING_SOFTWARE_EXCEPTION = 6,
    }
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64HypercallInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub r8: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub xmm_registers: [AlignedU128; 6],
    pub flags: HvHypercallInterceptMessageFlags,
    pub rsvd2: [u32; 3],
}

impl MessagePayload for HvX64HypercallInterceptMessage {}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64HypercallInterceptMessage {
    pub header: HvArm64InterceptMessageHeader,
    pub immediate: u16,
    pub reserved: u16,
    pub flags: HvHypercallInterceptMessageFlags,
    pub x: [u64; 18],
}

impl MessagePayload for HvArm64HypercallInterceptMessage {}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvHypercallInterceptMessageFlags {
    pub is_isolated: bool,
    #[bits(31)]
    _reserved: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64CpuidInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub default_result_rax: u64,
    pub default_result_rcx: u64,
    pub default_result_rdx: u64,
    pub default_result_rbx: u64,
}

impl MessagePayload for HvX64CpuidInterceptMessage {}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64ExceptionInfo {
    pub error_code_valid: bool,
    pub software_exception: bool,
    #[bits(6)]
    reserved: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64ExceptionInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub vector: u16,
    pub exception_info: HvX64ExceptionInfo,
    pub instruction_byte_count: u8,
    pub error_code: u32,
    pub exception_parameter: u64,
    pub reserved: u64,
    pub instruction_bytes: [u8; 16],
    pub ds_segment: HvX64SegmentRegister,
    pub ss_segment: HvX64SegmentRegister,
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
}

impl MessagePayload for HvX64ExceptionInterceptMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvInvalidVpRegisterMessage {
    pub vp_index: u32,
    pub reserved: u32,
}

impl MessagePayload for HvInvalidVpRegisterMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64ApicEoiMessage {
    pub vp_index: u32,
    pub interrupt_vector: u32,
}

impl MessagePayload for HvX64ApicEoiMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64UnrecoverableExceptionMessage {
    pub header: HvX64InterceptMessageHeader,
}

impl MessagePayload for HvX64UnrecoverableExceptionMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64HaltMessage {
    pub header: HvX64InterceptMessageHeader,
}

impl MessagePayload for HvX64HaltMessage {}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvArm64ResetInterceptMessage {
    pub header: HvArm64InterceptMessageHeader,
    pub reset_type: HvArm64ResetType,
    pub reset_code: u32,
}

impl MessagePayload for HvArm64ResetInterceptMessage {}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvArm64ResetType: u32 {
        POWER_OFF = 0,
        REBOOT = 1,
        SYSTEM_RESET = 2,
        HIBERNATE = 3,
    }
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, FromBytes)]
pub struct HvX64RegisterInterceptMessageFlags {
    pub is_memory_op: bool,
    #[bits(7)]
    _rsvd: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, FromBytes)]
pub struct HvX64RegisterInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub flags: HvX64RegisterInterceptMessageFlags,
    pub rsvd: u8,
    pub rsvd2: u16,
    pub register_name: HvX64RegisterName,
    pub access_info: HvX64RegisterAccessInfo,
}

#[repr(transparent)]
#[derive(IntoBytes, Immutable, FromBytes)]
pub struct HvX64RegisterAccessInfo(u128);

impl HvX64RegisterAccessInfo {
    pub fn new_source_value(source_value: HvRegisterValue) -> Self {
        Self(source_value.as_u128())
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvInterruptType : u32  {
        #![expect(non_upper_case_globals)]
        HvArm64InterruptTypeFixed = 0x0000,
        HvX64InterruptTypeFixed = 0x0000,
        HvX64InterruptTypeLowestPriority = 0x0001,
        HvX64InterruptTypeSmi = 0x0002,
        HvX64InterruptTypeRemoteRead = 0x0003,
        HvX64InterruptTypeNmi = 0x0004,
        HvX64InterruptTypeInit = 0x0005,
        HvX64InterruptTypeSipi = 0x0006,
        HvX64InterruptTypeExtInt = 0x0007,
        HvX64InterruptTypeLocalInt0 = 0x0008,
        HvX64InterruptTypeLocalInt1 = 0x0009,
    }
}

/// The declaration uses the fact the bits for the different
/// architectures don't intersect. When (if ever) they do,
/// will need to come up with a more elaborate abstraction.
/// The other possible downside is the lack of the compile-time
/// checks as adding that will require `guest_arch` support and
/// a large refactoring. To sum up, choosing expediency.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvInterruptControl {
    interrupt_type_value: u32,
    pub x86_level_triggered: bool,
    pub x86_logical_destination_mode: bool,
    pub arm64_asserted: bool,
    #[bits(29)]
    pub unused: u32,
}

impl HvInterruptControl {
    pub fn interrupt_type(&self) -> HvInterruptType {
        HvInterruptType(self.interrupt_type_value())
    }

    pub fn set_interrupt_type(&mut self, ty: HvInterruptType) {
        self.set_interrupt_type_value(ty.0)
    }

    pub fn with_interrupt_type(self, ty: HvInterruptType) -> Self {
        self.with_interrupt_type_value(ty.0)
    }
}

#[bitfield(u64)]
pub struct HvRegisterVsmCapabilities {
    pub dr6_shared: bool,
    pub mbec_vtl_mask: u16,
    pub deny_lower_vtl_startup: bool,
    pub supervisor_shadow_stack: bool,
    pub hardware_hvpt_available: bool,
    pub software_hvpt_available: bool,
    #[bits(6)]
    pub hardware_hvpt_range_bits: u8,
    pub intercept_page_available: bool,
    pub return_action_available: bool,
    /// If the VTL0 view of memory is mapped to the high address space, which is
    /// the highest legal physical address bit.
    ///
    /// Only available in VTL2.
    pub vtl0_alias_map_available: bool,
    /// If the [`HvRegisterVsmPartitionConfig`] register has support for
    /// `intercept_not_present`.
    ///
    /// Only available in VTL2.
    pub intercept_not_present_available: bool,
    pub install_intercept_ex: bool,
    /// Only available in VTL2.
    pub intercept_system_reset_available: bool,
    #[bits(1)]
    pub reserved1: u8,
    pub proxy_interrupt_redirect_available: bool,
    #[bits(29)]
    pub reserved2: u64,
}

#[bitfield(u64)]
pub struct HvRegisterVsmPartitionConfig {
    pub enable_vtl_protection: bool,
    #[bits(4)]
    pub default_vtl_protection_mask: u8,
    pub zero_memory_on_reset: bool,
    pub deny_lower_vtl_startup: bool,
    pub intercept_acceptance: bool,
    pub intercept_enable_vtl_protection: bool,
    pub intercept_vp_startup: bool,
    pub intercept_cpuid_unimplemented: bool,
    pub intercept_unrecoverable_exception: bool,
    pub intercept_page: bool,
    pub intercept_restore_partition_time: bool,
    /// The hypervisor will send all unmapped GPA intercepts to VTL2 rather than
    /// the host.
    pub intercept_not_present: bool,
    pub intercept_system_reset: bool,
    #[bits(48)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct HvRegisterVsmPartitionStatus {
    #[bits(16)]
    pub enabled_vtl_set: u16,
    #[bits(4)]
    pub maximum_vtl: u8,
    #[bits(16)]
    pub mbec_enabled_vtl_set: u16,
    #[bits(4)]
    pub supervisor_shadow_stack_enabled_vtl_set: u8,
    #[bits(24)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct HvRegisterGuestVsmPartitionConfig {
    #[bits(4)]
    pub maximum_vtl: u8,
    #[bits(60)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct HvRegisterVsmVpStatus {
    #[bits(4)]
    pub active_vtl: u8,
    pub active_mbec_enabled: bool,
    #[bits(11)]
    pub reserved_mbz0: u16,
    #[bits(16)]
    pub enabled_vtl_set: u16,
    #[bits(32)]
    pub reserved_mbz1: u32,
}

#[bitfield(u64)]
pub struct HvRegisterVsmCodePageOffsets {
    #[bits(12)]
    pub call_offset: u16,
    #[bits(12)]
    pub return_offset: u16,
    #[bits(40)]
    pub reserved: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvStimerState {
    pub undelivered_message_pending: u32,
    pub reserved: u32,
    pub config: u64,
    pub count: u64,
    pub adjustment: u64,
    pub undelivered_expiration_time: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvSyntheticTimersState {
    pub timers: [HvStimerState; 4],
    pub reserved: [u64; 5],
}

#[bitfield(u64)]
pub struct HvInternalActivityRegister {
    pub startup_suspend: bool,
    pub halt_suspend: bool,
    pub idle_suspend: bool,
    #[bits(61)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct HvSynicSint {
    pub vector: u8,
    _reserved: u8,
    pub masked: bool,
    pub auto_eoi: bool,
    pub polling: bool,
    _reserved2: bool,
    pub proxy: bool,
    #[bits(43)]
    _reserved2: u64,
}

#[bitfield(u64)]
pub struct HvSynicScontrol {
    pub enabled: bool,
    #[bits(63)]
    _reserved: u64,
}

#[bitfield(u64)]
pub struct HvSynicSimpSiefp {
    pub enabled: bool,
    #[bits(11)]
    _reserved: u64,
    #[bits(52)]
    pub base_gpn: u64,
}

#[bitfield(u64)]
pub struct HvSynicStimerConfig {
    pub enabled: bool,
    pub periodic: bool,
    pub lazy: bool,
    pub auto_enable: bool,
    // Note: On ARM64 the top 3 bits of apic_vector are reserved.
    pub apic_vector: u8,
    pub direct_mode: bool,
    #[bits(3)]
    pub _reserved1: u8,
    #[bits(4)]
    pub sint: u8,
    #[bits(44)]
    pub _reserved2: u64,
}

pub const HV_X64_PENDING_EVENT_EXCEPTION: u8 = 0;
pub const HV_X64_PENDING_EVENT_MEMORY_INTERCEPT: u8 = 1;
pub const HV_X64_PENDING_EVENT_NESTED_MEMORY_INTERCEPT: u8 = 2;
pub const HV_X64_PENDING_EVENT_VIRTUALIZATION_FAULT: u8 = 3;
pub const HV_X64_PENDING_EVENT_HYPERCALL_OUTPUT: u8 = 4;
pub const HV_X64_PENDING_EVENT_EXT_INT: u8 = 5;
pub const HV_X64_PENDING_EVENT_SHADOW_IPT: u8 = 6;

// Provides information about an exception.
#[bitfield(u128)]
pub struct HvX64PendingExceptionEvent {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    pub reserved0: u8,

    pub deliver_error_code: bool,
    #[bits(7)]
    pub reserved1: u8,
    pub vector: u16,
    pub error_code: u32,
    pub exception_parameter: u64,
}

/// Provides information about a virtualization fault.
#[bitfield(u128)]
pub struct HvX64PendingVirtualizationFaultEvent {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    pub reserved0: u8,

    pub reserved1: u8,
    pub parameter0: u16,
    pub code: u32,
    pub parameter1: u64,
}

/// Part of [`HvX64PendingEventMemoryIntercept`]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingEventMemoryInterceptPendingEventHeader {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    _reserved0: u8,
}

/// Part of [`HvX64PendingEventMemoryIntercept`]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingEventMemoryInterceptAccessFlags {
    /// Indicates if the guest linear address is valid.
    pub guest_linear_address_valid: bool,
    /// Indicates that the memory intercept was caused by an access to a guest physical address
    /// (instead of a page table as part of a page table walk).
    pub caused_by_gpa_access: bool,
    #[bits(6)]
    _reserved1: u8,
}

/// Provides information about a memory intercept.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingEventMemoryIntercept {
    pub event_header: HvX64PendingEventMemoryInterceptPendingEventHeader,
    /// VTL at which the memory intercept is targeted.
    /// Note: This field must be in Reg0.
    pub target_vtl: u8,
    /// Type of the memory access.
    pub access_type: HvInterceptAccessType,
    pub access_flags: HvX64PendingEventMemoryInterceptAccessFlags,
    pub _reserved2: u32,
    /// The guest linear address that caused the fault.
    pub guest_linear_address: u64,
    /// The guest physical address that caused the memory intercept.
    pub guest_physical_address: u64,
    pub _reserved3: u64,
}
const_assert!(size_of::<HvX64PendingEventMemoryIntercept>() == 0x20);

//
// Provides information about pending hypercall output.
//
#[bitfield(u128)]
pub struct HvX64PendingHypercallOutputEvent {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    pub reserved0: u8,

    // Whether the hypercall has been retired.
    pub retired: bool,

    #[bits(23)]
    pub reserved1: u32,

    // Indicates the number of bytes to be written starting from OutputGpa.
    pub output_size: u32,

    // Indicates the output GPA, which is not required to be page-aligned.
    pub output_gpa: u64,
}

// Provides information about a directly asserted ExtInt.
#[bitfield(u128)]
pub struct HvX64PendingExtIntEvent {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    pub reserved0: u8,
    pub vector: u8,
    #[bits(48)]
    pub reserved1: u64,
    pub reserved2: u64,
}

// Provides information about pending IPT shadowing.
#[bitfield(u128)]
pub struct HvX64PendingShadowIptEvent {
    pub event_pending: bool,
    #[bits(4)]
    pub event_type: u8,
    #[bits(59)]
    pub reserved0: u64,

    pub reserved1: u64,
}

#[bitfield(u128)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingEventReg0 {
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: u8,
    #[bits(4)]
    pub reserved: u8,
    #[bits(120)]
    pub data: u128,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingEvent {
    pub reg_0: HvX64PendingEventReg0,
    pub reg_1: AlignedU128,
}
const_assert!(size_of::<HvX64PendingEvent>() == 0x20);

impl From<HvX64PendingExceptionEvent> for HvX64PendingEvent {
    fn from(exception_event: HvX64PendingExceptionEvent) -> Self {
        HvX64PendingEvent {
            reg_0: HvX64PendingEventReg0::from(u128::from(exception_event)),
            reg_1: 0u128.into(),
        }
    }
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64PendingInterruptionRegister {
    pub interruption_pending: bool,
    #[bits(3)]
    pub interruption_type: u8,
    pub deliver_error_code: bool,
    #[bits(4)]
    pub instruction_length: u8,
    pub nested_event: bool,
    #[bits(6)]
    pub reserved: u8,
    pub interruption_vector: u16,
    pub error_code: u32,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64InterruptStateRegister {
    pub interrupt_shadow: bool,
    pub nmi_masked: bool,
    #[bits(62)]
    pub reserved: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvInstructionEmulatorHintsRegister {
    /// Indicates whether any secure VTL is enabled for the partition.
    pub partition_secure_vtl_enabled: bool,
    /// Indicates whether kernel or user execute control architecturally
    /// applies to execute accesses.
    pub mbec_user_execute_control: bool,
    #[bits(62)]
    pub _padding: u64,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HvAarch64PendingEventType: u8 {
        EXCEPTION = 0,
        SYNTHETIC_EXCEPTION = 1,
        HYPERCALL_OUTPUT = 2,
    }
}

// Support for bitfield structures.
impl HvAarch64PendingEventType {
    const fn from_bits(val: u8) -> Self {
        HvAarch64PendingEventType(val)
    }

    const fn into_bits(self) -> u8 {
        self.0
    }
}

#[bitfield[u8]]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64PendingEventHeader {
    #[bits(1)]
    pub event_pending: bool,
    #[bits(3)]
    pub event_type: HvAarch64PendingEventType,
    #[bits(4)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64PendingExceptionEvent {
    pub header: HvAarch64PendingEventHeader,
    pub _padding: [u8; 7],
    pub syndrome: u64,
    pub fault_address: u64,
}

#[bitfield[u8]]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64PendingHypercallOutputEventFlags {
    #[bits(1)]
    pub retired: u8,
    #[bits(7)]
    pub reserved: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64PendingHypercallOutputEvent {
    pub header: HvAarch64PendingEventHeader,
    pub flags: HvAarch64PendingHypercallOutputEventFlags,
    pub reserved: u16,
    pub output_size: u32,
    pub output_gpa: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64PendingEvent {
    pub header: HvAarch64PendingEventHeader,
    pub event_data: [u8; 15],
    pub _padding: [u64; 2],
}

#[bitfield(u32)]
#[derive(PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMapGpaFlags {
    pub readable: bool,
    pub writable: bool,
    pub kernel_executable: bool,
    pub user_executable: bool,
    pub supervisor_shadow_stack: bool,
    pub paging_writability: bool,
    pub verify_paging_writability: bool,
    #[bits(8)]
    _padding0: u32,
    pub adjustable: bool,
    #[bits(16)]
    _padding1: u32,
}

/// [`HvMapGpaFlags`] with no permissions set
pub const HV_MAP_GPA_PERMISSIONS_NONE: HvMapGpaFlags = HvMapGpaFlags::new();
pub const HV_MAP_GPA_PERMISSIONS_ALL: HvMapGpaFlags = HvMapGpaFlags::new()
    .with_readable(true)
    .with_writable(true)
    .with_kernel_executable(true)
    .with_user_executable(true);

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMonitorPage {
    pub trigger_state: HvMonitorTriggerState,
    pub reserved1: u32,
    pub trigger_group: [HvMonitorTriggerGroup; 4],
    pub reserved2: [u64; 3],
    pub next_check_time: [[u32; 32]; 4],
    pub latency: [[u16; 32]; 4],
    pub reserved3: [u64; 32],
    pub parameter: [[HvMonitorParameter; 32]; 4],
    pub reserved4: [u8; 1984],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMonitorPageSmall {
    pub trigger_state: HvMonitorTriggerState,
    pub reserved1: u32,
    pub trigger_group: [HvMonitorTriggerGroup; 4],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMonitorTriggerGroup {
    pub pending: u32,
    pub armed: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMonitorParameter {
    pub connection_id: u32,
    pub flag_number: u16,
    pub reserved: u16,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvMonitorTriggerState {
    #[bits(4)]
    pub group_enable: u32,
    #[bits(28)]
    pub reserved: u32,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvPmTimerInfo {
    #[bits(16)]
    pub port: u16,
    #[bits(1)]
    pub width_24: bool,
    #[bits(1)]
    pub enabled: bool,
    #[bits(14)]
    pub reserved1: u32,
    #[bits(32)]
    pub reserved2: u32,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64RegisterSevControl {
    pub enable_encrypted_state: bool,
    #[bits(11)]
    _rsvd1: u64,
    #[bits(52)]
    pub vmsa_gpa_page_number: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvRegisterReferenceTsc {
    pub enable: bool,
    #[bits(11)]
    pub reserved_p: u64,
    #[bits(52)]
    pub gpn: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvReferenceTscPage {
    pub tsc_sequence: u32,
    pub reserved1: u32,
    pub tsc_scale: u64,
    pub tsc_offset: i64,
    pub timeline_bias: u64,
    pub tsc_multiplier: u64,
    pub reserved2: [u64; 507],
}

pub const HV_REFERENCE_TSC_SEQUENCE_INVALID: u32 = 0;

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64VmgexitInterceptMessageFlags {
    pub ghcb_page_valid: bool,
    pub ghcb_request_error: bool,
    #[bits(62)]
    _reserved: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64VmgexitInterceptMessageGhcbPageStandard {
    pub ghcb_protocol_version: u16,
    _reserved: [u16; 3],
    pub sw_exit_code: u64,
    pub sw_exit_info1: u64,
    pub sw_exit_info2: u64,
    pub sw_scratch: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64VmgexitInterceptMessageGhcbPage {
    pub ghcb_usage: u32,
    _reserved: u32,
    pub standard: HvX64VmgexitInterceptMessageGhcbPageStandard,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64VmgexitInterceptMessage {
    pub header: HvX64InterceptMessageHeader,
    pub ghcb_msr: u64,
    pub flags: HvX64VmgexitInterceptMessageFlags,
    pub ghcb_page: HvX64VmgexitInterceptMessageGhcbPage,
}

impl MessagePayload for HvX64VmgexitInterceptMessage {}

#[bitfield(u64)]
pub struct HvRegisterVpAssistPage {
    pub enabled: bool,
    #[bits(11)]
    _reserved: u64,
    #[bits(52)]
    pub gpa_page_number: u64,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64RegisterPageDirtyFlags {
    pub general_purpose: bool,
    pub instruction_pointer: bool,
    pub xmm: bool,
    pub segments: bool,
    pub flags: bool,
    #[bits(27)]
    reserved: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvX64RegisterPage {
    pub version: u16,
    pub is_valid: u8,
    pub vtl: u8,
    pub dirty: HvX64RegisterPageDirtyFlags,
    pub gp_registers: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    pub reserved: u64,
    pub xmm: [u128; 6],
    pub segment: [u128; 6],
    // Misc. control registers (cannot be set via this interface).
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub dr7: u64,
    pub pending_interruption: HvX64PendingInterruptionRegister,
    pub interrupt_state: HvX64InterruptStateRegister,
    pub instruction_emulation_hints: HvInstructionEmulatorHintsRegister,
    pub reserved_end: [u8; 3672],
}

const _: () = assert!(size_of::<HvX64RegisterPage>() == HV_PAGE_SIZE_USIZE);

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64RegisterPageDirtyFlags {
    _unused: bool,
    pub instruction_pointer: bool,
    pub processor_state: bool,
    pub control_registers: bool,
    #[bits(28)]
    reserved: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HvAarch64RegisterPage {
    pub version: u16,
    pub is_valid: u8,
    pub vtl: u8,
    pub dirty: HvAarch64RegisterPageDirtyFlags,
    // Reserved.
    pub _rsvd: [u64; 33],
    // Instruction pointer.
    pub pc: u64,
    // Processor state.
    pub cpsr: u64,
    // Control registers.
    pub sctlr_el1: u64,
    pub tcr_el1: u64,
    // Reserved.
    pub reserved_end: [u8; 3792],
}

const _: () = assert!(size_of::<HvAarch64RegisterPage>() == HV_PAGE_SIZE_USIZE);

#[bitfield(u64)]
pub struct HvRegisterVsmWpWaitForTlbLock {
    pub wait: bool,
    #[bits(63)]
    _reserved: u64,
}

#[bitfield(u64)]
pub struct HvRegisterVsmVpSecureVtlConfig {
    pub mbec_enabled: bool,
    pub tlb_locked: bool,
    pub supervisor_shadow_stack_enabled: bool,
    pub hardware_hvpt_enabled: bool,
    #[bits(60)]
    _reserved: u64,
}

#[bitfield(u64)]
pub struct HvRegisterCrInterceptControl {
    pub cr0_write: bool,
    pub cr4_write: bool,
    pub xcr0_write: bool,
    pub ia32_misc_enable_read: bool,
    pub ia32_misc_enable_write: bool,
    pub msr_lstar_read: bool,
    pub msr_lstar_write: bool,
    pub msr_star_read: bool,
    pub msr_star_write: bool,
    pub msr_cstar_read: bool,
    pub msr_cstar_write: bool,
    pub apic_base_msr_read: bool,
    pub apic_base_msr_write: bool,
    pub msr_efer_read: bool,
    pub msr_efer_write: bool,
    pub gdtr_write: bool,
    pub idtr_write: bool,
    pub ldtr_write: bool,
    pub tr_write: bool,
    pub msr_sysenter_cs_write: bool,
    pub msr_sysenter_eip_write: bool,
    pub msr_sysenter_esp_write: bool,
    pub msr_sfmask_write: bool,
    pub msr_tsc_aux_write: bool,
    pub msr_sgx_launch_control_write: bool,
    pub msr_xss_write: bool,
    pub msr_scet_write: bool,
    pub msr_pls_ssp_write: bool,
    pub msr_interrupt_ssp_table_addr_write: bool,
    #[bits(35)]
    _rsvd_z: u64,
}
