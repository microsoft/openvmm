// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU fingerprint of the WHP backend: the guest CPU surface and time
//! capabilities that the Windows Hypervisor Platform supports on this host.

use crate::Error;
use crate::WhpResultExt;
use cpu_profile::cpuid::CpuidEntry;
use cpu_profile::fingerprint::BackendFingerprint;
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
    if let Some(features) = available {
        match probe_partition(Some(features)) {
            Ok(probe) => partition = Some(probe),
            Err(error) => rejected_features = Some(error),
        }
    }
    let partition = match partition {
        Some(partition) => partition,
        None => probe_partition(None)?,
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

/// Creates a probe partition with one virtual processor and the
/// in-hypervisor x2APIC. `features` are the processor and XSAVE features to
/// enable, or `None` for WHP's default processor features.
fn probe_partition(
    features: Option<(
        whp::ProcessorFeatures,
        whp::abi::WHV_PROCESSOR_XSAVE_FEATURES,
    )>,
) -> Result<whp::Partition, Error> {
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
    if let Some((processor, xsave)) = features {
        config
            .set_property(whp::PartitionProperty::ProcessorFeatures(processor))
            .for_op("set the probe partition processor features")?;
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
}
