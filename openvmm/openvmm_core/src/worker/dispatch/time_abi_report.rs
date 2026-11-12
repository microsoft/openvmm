// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::HostClockStatus;
#[cfg(guest_arch = "x86_64")]
use super::IdentityMsrRoute;
#[cfg(guest_arch = "x86_64")]
use super::NativeRateBasis;
#[cfg(guest_arch = "x86_64")]
use super::RateCheck;
#[cfg(guest_arch = "x86_64")]
use super::TscSyncMethod;
use inspect::Inspect;

/// Completed CPU/time admission, exposed at the shared `time-abi` inspect
/// node. Architecture-specific evidence is never synthesized for another ISA.
#[derive(Debug, Clone, Inspect)]
pub(in crate::worker::dispatch) struct TimeAbiReport {
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

// Keep each adapter's existing pairing/bound representation policy.
pub(in crate::worker::dispatch) fn published_time_fields(
    sample: &virt::time_abi::publish::PublishedSample,
    pairing_ns: u32,
    bound: impl Fn(u64) -> Option<u32>,
) -> chipset_resources::microvm_time::HostTimeFields {
    use chipset_resources::microvm_time::HostClockStatus as SampleStatus;

    chipset_resources::microvm_time::HostTimeFields {
        utc_ns: sample.utc_ns,
        tsc: sample.tsc,
        pairing_ns,
        status: match sample.status {
            HostClockStatus::Unknown => SampleStatus::Unknown,
            HostClockStatus::Synchronized => SampleStatus::Synchronized,
            HostClockStatus::FreeRunning => SampleStatus::FreeRunning,
            HostClockStatus::Unreliable => SampleStatus::Unreliable,
        },
        disruption: sample.disruption,
        correction: sample.correction,
        correction_bound: sample.correction_bound.and_then(&bound),
        max_error_us: sample.max_error_us.and_then(bound),
    }
}
