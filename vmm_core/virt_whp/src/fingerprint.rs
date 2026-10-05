// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU fingerprint of the WHP backend: the guest CPU surface and time
//! capabilities that the Windows Hypervisor Platform supports on this host.

use crate::Error;
use crate::WhpResultExt;
use crate::profile_features::WhpFeatures;
use crate::profile_features::profile_features;
use crate::time_abi::host_cpuid;
use crate::time_abi::processor_features;
use crate::time_abi::unlisted_cpuid;
use cpu_profile::CpuProfile;
use cpu_profile::cpuid::CpuidEntry;
use cpu_profile::fingerprint::BackendFingerprint;
use whp::abi::WHV_PROCESSOR_FEATURES;
use whp::abi::WHV_PROCESSOR_FEATURES1;
use whp::abi::WHV_PROCESSOR_XSAVE_FEATURES;
use whp::abi::WHV_X64_MSR_EXIT_BITMAP;

const METHOD: &str = "WHvGetVirtualProcessorCpuidOutput on a probe partition with every processor \
     and XSAVE feature that WHP reports as available, and an in-hypervisor x2APIC";

/// The TSC value written to the probe virtual processor. A probe partition
/// starts near zero, so a read-back between this value and twice it shows
/// that the write took effect.
const PROBE_TSC: u64 = 1 << 40;

/// The MSR exits that WHP can deliver, by `WHV_X64_MSR_EXIT_BITMAP` name.
const MSR_EXITS: &[(&str, WHV_X64_MSR_EXIT_BITMAP)] = &[
    ("UnhandledMsrs", WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs),
    ("TscMsrWrite", WHV_X64_MSR_EXIT_BITMAP::TscMsrWrite),
    ("TscMsrRead", WHV_X64_MSR_EXIT_BITMAP::TscMsrRead),
    (
        "ApicBaseMsrWrite",
        WHV_X64_MSR_EXIT_BITMAP::ApicBaseMsrWrite,
    ),
    (
        "MiscEnableMsrRead",
        WHV_X64_MSR_EXIT_BITMAP::MiscEnableMsrRead,
    ),
    (
        "McUpdatePatchLevelMsrRead",
        WHV_X64_MSR_EXIT_BITMAP::McUpdatePatchLevelMsrRead,
    ),
];

/// Returns the guest CPU surface and time capabilities that WHP supports on
/// this host.
///
/// The CPUID table is what the virtual processor of a transient probe
/// partition reports. The probe partition enables every processor and XSAVE
/// feature that WHP reports as available, because WHP's default processor
/// features, which OpenVMM's partitions use today, omit some of them, such
/// as the speculation controls of `CPUID.(7,0):EDX` and PSFD. If WHP rejects
/// the available features, the probe partition falls back to the default
/// features and the fingerprint records the error. The probe partition uses
/// the in-hypervisor x2APIC, has no memory, never runs, and is deleted
/// before this returns. The fingerprint also records the processor
/// capabilities (0x1000 through 0x1009), probes per-VP TSC writes and
/// partition time suspension on the probe partition, and probes TSC
/// frequency virtualization on a second partition object that is never set
/// up.
pub fn cpu_fingerprint() -> Result<BackendFingerprint, Error> {
    let available = whp::capabilities::processor_features()
        .ok()
        .zip(whp::capabilities::processor_xsave_features().ok());
    let mut rejected_features = None;
    let mut partition = None;
    if let Some((processor, xsave)) = available {
        match probe_partition(ProbeFeatures::Available(processor, xsave)) {
            Ok(probe) => partition = Some(probe),
            Err(error) => rejected_features = Some(error),
        }
    }
    let partition = match partition {
        Some(partition) => partition,
        None => probe_partition(ProbeFeatures::Default)?,
    };
    let cpuid = probe_cpuid(&partition)?;

    let mut fingerprint = BackendFingerprint::new("whp", METHOD, cpuid);
    if let Some(error) = rejected_features {
        fingerprint.set_unavailable("whp.probe.ProcessorFeatures", &error);
    }
    record_capabilities(&mut fingerprint);
    let vp = partition.vp(0);

    let tsc_frequency_hz = fingerprint.record(
        "whp.probe.ProcessorClockFrequency",
        partition.tsc_frequency(),
    );
    let lapic_timer_frequency_hz = fingerprint.record(
        "whp.probe.InterruptClockFrequency",
        partition.apic_frequency(),
    );
    fingerprint.record(
        "whp.probe.PhysicalAddressWidth",
        partition.physical_address_width().map(u64::from),
    );

    let tsc_write = (|| {
        vp.set_register(whp::Register64::Tsc, PROBE_TSC)?;
        vp.get_register(whp::Register64::Tsc)
    })();
    let tsc_offset_control = match tsc_write {
        Ok(tsc) => tsc.wrapping_sub(PROBE_TSC) < PROBE_TSC,
        Err(error) => {
            fingerprint.set_unavailable("whp.probe.tsc_write", &error);
            false
        }
    };
    let time_freeze = match partition
        .suspend_time()
        .and_then(|()| partition.resume_time())
    {
        Ok(()) => true,
        Err(error) => {
            fingerprint.set_unavailable("whp.probe.suspend_time", &error);
            false
        }
    };
    drop(partition);

    let tsc_scaling =
        tsc_frequency_hz.map(|frequency_hz| probe_tsc_scaling(frequency_hz, &mut fingerprint));
    let msr_exits = fingerprint
        .values
        .get("whp.capability.X64MsrExitBitmap")
        .map(|bitmap| bitmap.0);
    let exits = fingerprint
        .values
        .get("whp.capability.ExtendedVmExits")
        .map(|exits| exits.0);

    let time = &mut fingerprint.time;
    time.tsc_frequency_hz = tsc_frequency_hz;
    time.lapic_timer_frequency_hz = lapic_timer_frequency_hz;
    time.tsc_offset_control = Some(tsc_offset_control);
    time.time_freeze = Some(time_freeze);
    time.tsc_scaling = tsc_scaling;
    if let Some(exits) = exits {
        time.msr_intercepts.insert(
            "X64MsrExit".to_owned(),
            whp::abi::WHV_EXTENDED_VM_EXITS(exits)
                .is_set(whp::abi::WHV_EXTENDED_VM_EXITS::X64MsrExit),
        );
    }
    if let Some(msr_exits) = msr_exits {
        for &(name, bit) in MSR_EXITS {
            time.msr_intercepts.insert(
                name.to_owned(),
                WHV_X64_MSR_EXIT_BITMAP(msr_exits).is_set(bit),
            );
        }
    }
    Ok(fingerprint)
}

/// Returns what VP 0 of a transient probe partition configured from
/// `profile` reads at every entry of the host's CPUID outside the profile's
/// tables ([`cpu_profile::unlisted_cpuid_candidates`]), each read at
/// subleaf 0 if subleaf-independent, for the `--cpu-fingerprint` check that
/// those entries read zero (`E_CPU_UNLISTED`).
///
/// It measures what a time ABI cold boot checks on VP 0 of its partition.
/// The probe partition's processor feature banks and XSAVE features derive
/// from the profile as `WhpTimeAbi::configure` derives them, without the
/// features that the time ABI hides, so that, unlike [`cpu_fingerprint`]'s
/// probe partition with every available feature, it does not present the
/// XSAVE components of features that no profile enables, such as CET's. It
/// programs no CPUID results, which cover only the profile's own entries.
/// The partition has no memory, never runs, and is deleted before this
/// returns.
///
/// Fails with `E_PROFILE_UNSUPPORTED` if WHP cannot present the profile's
/// features.
pub fn profile_unlisted_cpuid(profile: &CpuProfile) -> Result<Vec<CpuidEntry>, Error> {
    let available =
        whp::capabilities::processor_features().for_op("query the processor features")?;
    let available_xsave =
        whp::capabilities::processor_xsave_features().for_op("query the XSAVE features")?;
    let derived = profile_features(
        profile,
        WhpFeatures {
            banks: [available.bank0.0, available.bank1.0],
            xsave: available_xsave.0,
        },
    )?;
    let mut features = available;
    features.bank0 = WHV_PROCESSOR_FEATURES(derived.banks[0]);
    features.bank1 = WHV_PROCESSOR_FEATURES1(derived.banks[1]);
    let partition = probe_partition(ProbeFeatures::Profile(
        processor_features(features),
        WHV_PROCESSOR_XSAVE_FEATURES(derived.xsave),
    ))?;
    let vp = partition.vp(0);
    let leaves = unlisted_cpuid(profile, host_cpuid(), |leaf, subleaf| {
        vp.get_cpuid_output(leaf, subleaf)
            .map(|output| [output.Eax, output.Ebx, output.Ecx, output.Edx])
    })
    .for_op("query the probe virtual processor CPUID")?;
    Ok(leaves
        .into_iter()
        .map(|leaf| CpuidEntry::new(leaf.function, leaf.index, leaf.result))
        .collect())
}

/// The processor and XSAVE features of a probe partition.
enum ProbeFeatures {
    /// WHP's default processor features.
    Default,
    /// Every processor and XSAVE feature that WHP reports as available.
    Available(whp::ProcessorFeatures, WHV_PROCESSOR_XSAVE_FEATURES),
    /// The features of a CPU profile's time ABI partition, set through the
    /// feature banks as a time ABI partition sets them.
    Profile(whp::ProcessorFeatures, WHV_PROCESSOR_XSAVE_FEATURES),
}

/// Creates a probe partition with one virtual processor, the in-hypervisor
/// x2APIC, and `features`.
fn probe_partition(features: ProbeFeatures) -> Result<whp::Partition, Error> {
    let mut config =
        whp::PartitionConfig::new().for_op("create the fingerprint probe partition")?;
    config
        .set_property(whp::PartitionProperty::ProcessorCount(1))
        .for_op("set the probe partition processor count")?;
    config
        .set_property(whp::PartitionProperty::LocalApicEmulationMode(
            whp::abi::WHvX64LocalApicEmulationModeX2Apic,
        ))
        .for_op("set the probe partition APIC emulation mode")?;
    let (processor, xsave) = match features {
        ProbeFeatures::Default => (None, None),
        ProbeFeatures::Available(processor, xsave) => (
            Some(whp::PartitionProperty::ProcessorFeatures(processor)),
            Some(xsave),
        ),
        ProbeFeatures::Profile(processor, xsave) => (
            Some(whp::PartitionProperty::ProcessorFeaturesBanks(processor)),
            Some(xsave),
        ),
    };
    if let Some(processor) = processor {
        config
            .set_property(processor)
            .for_op("set the probe partition processor features")?;
    }
    if let Some(xsave) = xsave {
        config
            .set_property(whp::PartitionProperty::ProcessorXsaveFeatures(xsave))
            .for_op("set the probe partition XSAVE features")?;
    }
    let partition = config
        .create()
        .for_op("set up the fingerprint probe partition")?;
    partition
        .create_vp(0)
        .create()
        .for_op("create the probe virtual processor")?;
    Ok(partition)
}

/// Returns the CPUID table that the virtual processor of the probe
/// partition reports.
fn probe_cpuid(partition: &whp::Partition) -> Result<Vec<CpuidEntry>, Error> {
    let vp = partition.vp(0);
    cpu_profile::cpuid::enumerate(|leaf, subleaf| {
        vp.get_cpuid_output(leaf, subleaf)
            .map(|output| [output.Eax, output.Ebx, output.Ecx, output.Edx])
    })
    .for_op("query the probe virtual processor CPUID")
}

/// Records the WHP capabilities: the processor feature banks as feature
/// banks, and the others as values.
fn record_capabilities(fingerprint: &mut BackendFingerprint) {
    use whp::capabilities;

    match capabilities::processor_features_banks() {
        Ok(banks) => {
            fingerprint.set_value(
                "whp.capability.ProcessorFeaturesBanks.count",
                banks.BanksCount.into(),
            );
            for (index, bank) in banks.Banks.iter().enumerate() {
                fingerprint.set_feature_bank(
                    format!("whp.capability.ProcessorFeaturesBanks.bank{index}"),
                    *bank,
                );
            }
        }
        Err(error) => fingerprint.set_unavailable("whp.capability.ProcessorFeaturesBanks", &error),
    }
    match capabilities::processor_features_bank0() {
        Ok(features) => {
            fingerprint.set_feature_bank("whp.capability.ProcessorFeatures", features.0)
        }
        Err(error) => fingerprint.set_unavailable("whp.capability.ProcessorFeatures", &error),
    }
    match capabilities::processor_xsave_features() {
        Ok(features) => {
            fingerprint.set_feature_bank("whp.capability.ProcessorXsaveFeatures", features.0)
        }
        Err(error) => fingerprint.set_unavailable("whp.capability.ProcessorXsaveFeatures", &error),
    }
    match capabilities::synthetic_processor_features_banks() {
        Ok(banks) => {
            fingerprint.set_value(
                "whp.capability.SyntheticProcessorFeaturesBanks.count",
                banks.BanksCount.into(),
            );
            for (index, bank) in banks.Banks.iter().enumerate() {
                fingerprint.set_value(
                    format!("whp.capability.SyntheticProcessorFeaturesBanks.bank{index}"),
                    *bank,
                );
            }
        }
        Err(error) => {
            fingerprint.set_unavailable("whp.capability.SyntheticProcessorFeaturesBanks", &error)
        }
    }
    match capabilities::processor_frequency_cap() {
        Ok(cap) => {
            for (name, value) in [
                ("Flags", cap.Flags),
                ("HighestFrequencyMhz", cap.HighestFrequencyMhz),
                ("NominalFrequencyMhz", cap.NominalFrequencyMhz),
                ("LowestFrequencyMhz", cap.LowestFrequencyMhz),
                ("FrequencyStepMhz", cap.FrequencyStepMhz),
            ] {
                fingerprint.set_value(
                    format!("whp.capability.ProcessorFrequencyCap.{name}"),
                    value.into(),
                );
            }
        }
        Err(error) => fingerprint.set_unavailable("whp.capability.ProcessorFrequencyCap", &error),
    }
    fingerprint.record(
        "whp.capability.PerfmonFeatures",
        capabilities::perfmon_features().map(|features| features.0),
    );
    fingerprint.record(
        "whp.capability.Features",
        capabilities::features().map(|features| features.0),
    );
    fingerprint.record(
        "whp.capability.ExtendedVmExits",
        capabilities::extended_vm_exits().map(|exits| exits.0),
    );
    fingerprint.record(
        "whp.capability.X64MsrExitBitmap",
        capabilities::x64_msr_exit_bitmap().map(|bitmap| bitmap.0),
    );
    fingerprint.record(
        "whp.capability.ProcessorVendor",
        capabilities::processor_vendor().map(|vendor| vendor.0.into()),
    );
    fingerprint.record(
        "whp.capability.ProcessorClFlushSize",
        capabilities::processor_cl_flush_size().map(u64::from),
    );
    fingerprint.record(
        "whp.capability.ProcessorClockFrequency",
        capabilities::processor_clock_frequency(),
    );
    fingerprint.record(
        "whp.capability.InterruptClockFrequency",
        capabilities::interrupt_clock_frequency(),
    );
}

/// Asks WHP to virtualize half of the host TSC frequency for a partition
/// that is never set up, and returns whether WHP accepted it.
fn probe_tsc_scaling(host_frequency_hz: u64, fingerprint: &mut BackendFingerprint) -> bool {
    let result = whp::PartitionConfig::new().and_then(|mut config| {
        config
            .set_property(whp::PartitionProperty::ProcessorCount(1))?
            .set_property(whp::PartitionProperty::ProcessorClockFrequency(
                host_frequency_hz / 2,
            ))?;
        Ok(())
    });
    match result {
        Ok(()) => true,
        Err(error) => {
            fingerprint.set_unavailable("whp.probe.ProcessorClockFrequency.set", &error);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cpu_fingerprint;
    use super::profile_unlisted_cpuid;
    use crate::time_abi::host_cpuid;
    use cpu_profile::cpuid;
    use test_with_tracing::test;

    #[test]
    #[ignore = "requires WHP"]
    fn fingerprint_reports_the_supported_surface() {
        let fingerprint = cpu_fingerprint().unwrap();
        tracing::info!(unavailable = ?fingerprint.unavailable, "WHP fingerprint");
        assert_eq!(fingerprint, cpu_fingerprint().unwrap());
        assert!(cpuid::lookup(&fingerprint.cpuid, 0, 0).is_some());
        assert_eq!(fingerprint.xsave.xcr0_supported.0 & 0x3, 0x3);
        assert!(fingerprint.time.tsc_frequency_hz.is_some());
        assert!(fingerprint.time.lapic_timer_frequency_hz.is_some());
        assert_eq!(fingerprint.time.tsc_offset_control, Some(true));
        // WHP accepts every feature that it reports as available.
        assert!(
            !fingerprint
                .unavailable
                .contains_key("whp.probe.ProcessorFeatures")
        );
    }

    /// The fingerprint check reads the entries outside the host's profile on
    /// a partition configured from the profile, as a cold boot does: they
    /// read zero there, even where the probe partition with every available
    /// feature presents host data, such as the CET XSAVE components of a
    /// CET-capable host.
    #[test]
    #[ignore = "requires WHP"]
    fn a_partition_of_the_host_profile_reads_zero_outside_it() {
        let profile = match cpu_profile::select_auto(&cpu_profile::HostCpuSignature::current()) {
            Ok(profile) => profile,
            Err(err) => {
                println!("skipped: no profile for this host: {err}");
                return;
            }
        };
        let presented = profile_unlisted_cpuid(profile).unwrap();
        let candidates = cpu_profile::unlisted_cpuid_candidates(profile, host_cpuid());
        assert_eq!(
            presented
                .iter()
                .map(cpuid::CpuidEntry::key)
                .collect::<Vec<_>>(),
            candidates
        );
        cpu_profile::check_unlisted_cpuid(profile, &presented).unwrap();
        let fingerprint = cpu_fingerprint().unwrap();
        let all_features = cpu_profile::unlisted_cpuid_violations(profile, &fingerprint.cpuid);
        println!(
            "{}: {} host entries outside the profile read zero on its partition; the probe \
             partition with every feature presents {all_features:?}",
            profile.id(),
            candidates.len()
        );
    }
}
