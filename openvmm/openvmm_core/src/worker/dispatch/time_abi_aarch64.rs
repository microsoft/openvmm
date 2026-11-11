// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ARM adaptation of the existing time-sample source and host publisher.
//!
//! This is not a second time service. The backend supplies actual counter/UTC
//! pairs; the existing publisher tracks corrections/disruptions and the
//! existing platform device encodes the v1 sample. No guest-TSC offset,
//! implicit profile, or host-counter substitute is used.

use crate::partition::HvlitePartition;
use chipset_resources::microvm_time::ARM64_TIME_BINDING_LEN;
use chipset_resources::microvm_time::HostTimeFields;
use chipset_resources::microvm_time::TimeSampleSource;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::Weak;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiTestHooks;
use virt::time_abi::publish::HostClockPublisher;
use virt::time_abi::publish::NativeRateBasis;
use virt::time_abi::publish::PairedReadings;
use virt::time_abi::publish::SampleRates;

pub(super) type PartitionTimeAbi = virt::time_abi::TimeAbiConfig;

pub(super) struct TimeAbiState {
    pub clock: Arc<TimeAbiClock>,
}

pub(super) fn prepare_contract(
    parameters: &openvmm_defs::time_abi::TimeAbiParameters,
    hypervisor: &str,
    restored: Option<&openvmm_defs::time_abi::SnapshotCpuProfile>,
    _topology: &vm_topology::processor::ProcessorTopology,
) -> anyhow::Result<(PartitionTimeAbi, virt::time_abi::TimeAbiConfig)> {
    anyhow::ensure!(
        !parameters.hooks.active(),
        "ARM CPU/time test hooks are not supported"
    );
    anyhow::ensure!(
        restored.is_none(),
        "ARM CPU/time profiles do not support restore"
    );
    let native = super::probe_cpu_fingerprint(hypervisor)?.into_aarch64()?;
    let profile = cpu_profile::aarch64::select(&parameters.cpu_profile, &native)?;
    if cpu_profile::is_host_profile_id(&profile.id) {
        tracing::warn!(
            requested = %parameters.cpu_profile,
            profile = %profile.id,
            "using a policy-filtered native ARM host profile; firmware or hypervisor updates can change its CPU identity"
        );
    }
    tracing::info!(
        profile = %profile.id,
        digest = %profile.sha256.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        native_counter_hz = native.counter_hz,
        backend_usable_gpa_bits = native.backend_usable_gpa_bits,
        architectural_pa_bits = profile.physical_address_bits,
        "selected ARM CPU/time contract"
    );
    let config = virt::time_abi::TimeAbiConfig {
        cpu_profile: profile,
        counter_hz: native.counter_hz,
        backend_usable_gpa_bits: native.backend_usable_gpa_bits,
    };
    config.validate()?;
    Ok((config.clone(), config))
}

pub(super) fn admit_partition(
    prepared: PartitionTimeAbi,
    context: super::TimeAbiAdmission<'_>,
) -> anyhow::Result<TimeAbiState> {
    Ok(TimeAbiState {
        clock: Arc::new(TimeAbiClock::new(
            context.partition,
            context.capacity,
            &prepared,
            &context.parameters.hooks,
        )?),
    })
}

pub(super) struct TimeAbiClock {
    // The device/source must not keep the partition alive during owned teardown.
    partition: Weak<dyn HvlitePartition>,
    binding: [u8; ARM64_TIME_BINDING_LEN],
    rates: SampleRates,
    publisher: Mutex<HostClockPublisher>,
}

impl TimeAbiClock {
    pub(super) fn new(
        partition: &Arc<dyn HvlitePartition>,
        capacity: u32,
        expected: &virt::time_abi::TimeAbiConfig,
        hooks: &TimeAbiTestHooks,
    ) -> Result<Self, TimeAbiError> {
        let backend = partition.time_abi().ok_or_else(|| TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            "no ARM CPU/time backend contract was selected; canonical profile and effective guest validation gates remain closed",
        ))?;
        let config = backend.configuration();
        config.validate()?;
        virt::time_abi::aarch64::verify_cpu_id_requirements(backend)?;
        if config != expected {
            return Err(TimeAbiError::new(
                TimeAbiCode::CpuSurface,
                format!(
                    "ARM backend CPU/time contract differs from prepared profile '{}'",
                    expected.cpu_profile.id
                ),
            ));
        }
        Ok(Self {
            partition: Arc::downgrade(partition),
            binding: config.binding(capacity)?,
            rates: SampleRates {
                declared_hz: config.counter_hz,
                native_hz: config.counter_hz,
                native_basis: NativeRateBasis::ArchitecturalCounter,
            },
            publisher: Mutex::new(HostClockPublisher::new(
                virt::time_abi::host::host_clock_kind(),
                hooks,
            )),
        })
    }

    fn try_sample(&self) -> Result<HostTimeFields, TimeAbiError> {
        let partition = self.partition.upgrade().ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "ARM partition was released before the time sample",
            )
        })?;
        let backend = partition.time_abi().ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "ARM partition has no CPU/time contract",
            )
        })?;
        let mut publisher = self.publisher.lock();
        let pair = backend.counter_utc_pair()?;
        let sample = publisher.publish_paired(
            &PairedReadings {
                guest_counter: pair.counter.ticks,
                counter_epoch: Some(pair.counter.epoch),
                time: pair.time,
                pairing_ns: pair.pairing_ns,
                discipline: virt::time_abi::host::host_clock_discipline(),
                suspended_ns: virt::time_abi::host::host_suspended_ns()?,
            },
            self.rates,
        );
        // An unrepresentable bound is unknown, never an understated finite bound.
        let bound = |value| u32::try_from(value).ok().filter(|value| *value != u32::MAX);
        let pairing_ns = u32::try_from(sample.pairing_ns).map_err(|_| {
            TimeAbiError::new(
                TimeAbiCode::ProfileTimeBits,
                "ARM counter pairing does not fit the v1 sample",
            )
        })?;
        Ok(super::published_time_fields(&sample, pairing_ns, bound))
    }
}

impl TimeSampleSource for TimeAbiClock {
    fn sample(&self) -> Option<HostTimeFields> {
        match self.try_sample() {
            Ok(sample) => Some(sample),
            Err(error) => {
                tracelimit::error_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "ARM time ABI time sample failed",
                );
                None
            }
        }
    }

    fn arm64_time_binding(&self) -> Option<[u8; ARM64_TIME_BINDING_LEN]> {
        Some(self.binding)
    }
}
