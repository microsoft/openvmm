// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;
#[cfg(guest_arch = "x86_64")]
use virt::time_abi::IdentityMsrRoute;
#[cfg(guest_arch = "x86_64")]
use virt::time_abi::RateCheck;
#[cfg(guest_arch = "x86_64")]
use virt::time_abi::TscSyncMethod;
#[cfg(guest_arch = "x86_64")]
use virt::time_abi::publish::NativeRateBasis;

/// Completed CPU/time admission, exposed at the shared `time-abi` inspect
/// node. Architecture-specific evidence is never synthesized for another ISA.
#[derive(Debug, Clone, Inspect)]
pub(super) struct TimeAbiReport {
    pub hypervisor: String,
    pub cpu_profile: String,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(debug)]
    pub msr_route: IdentityMsrRoute,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(debug)]
    pub sync: TscSyncMethod,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(debug)]
    pub native_rate: NativeRateBasis,
    #[cfg(guest_arch = "x86_64")]
    pub native_tsc_hz: u64,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(debug)]
    pub rate_check: Option<RateCheck>,
    #[cfg(guest_arch = "aarch64")]
    pub cpu_profile_sha256: String,
    #[cfg(guest_arch = "aarch64")]
    pub counter_hz: u64,
    #[cfg(guest_arch = "aarch64")]
    pub backend_usable_gpa_bits: u32,
    #[cfg(guest_arch = "aarch64")]
    pub configured_gpa_bits: u32,
    #[cfg(guest_arch = "aarch64")]
    pub architectural_pa_bits: u8,
    #[cfg(guest_arch = "aarch64")]
    pub effective_parange: u8,
    #[cfg(guest_arch = "aarch64")]
    pub feature_bank_count: u32,
    #[cfg(guest_arch = "aarch64")]
    pub feature_bank0: u64,
    #[cfg(guest_arch = "aarch64")]
    pub feature_bank1: u64,
    #[cfg(guest_arch = "aarch64")]
    pub cpu_id_registers_verified: usize,
    #[cfg(guest_arch = "aarch64")]
    pub backend_readback: bool,
    #[cfg(guest_arch = "aarch64")]
    pub effective_cpu: bool,
}
