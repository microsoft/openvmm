// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU fingerprint of the MSHV backend: the guest CPU surface and time
//! capabilities that the Microsoft hypervisor supports on this host.

use crate::Error;
use crate::ErrorInner;
use crate::KernelError;
use crate::LinuxMshv;
use crate::VcpuFdExt;
use crate::create_vm_with_retry;
use cpu_profile::fingerprint::BackendFingerprint;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;

const METHOD: &str = "HvCallGetVpCpuidValues on a probe partition that enables every processor \
     and XSAVE feature of the host partition";

/// The host partition properties that the probe partition enables, which
/// together with its CPUID define the guest CPU surface.
const FEATURE_BANKS: [HvPartitionPropertyCode; 3] = [
    HvPartitionPropertyCode::ProcessorFeatures0,
    HvPartitionPropertyCode::ProcessorFeatures1,
    HvPartitionPropertyCode::ProcessorXsaveFeatures,
];

/// Other host partition properties recorded in the fingerprint.
const HOST_PROPERTIES: &[HvPartitionPropertyCode] = &[
    HvPartitionPropertyCode::ProcessorVendor,
    HvPartitionPropertyCode::ProcessorClockFrequency,
    HvPartitionPropertyCode::ProcessorCLFlushSize,
    HvPartitionPropertyCode::PhysicalAddressWidth,
    HvPartitionPropertyCode::XsaveStates,
    HvPartitionPropertyCode::MaxXsaveDataSize,
    HvPartitionPropertyCode::FeatureBankCount,
    HvPartitionPropertyCode::RootProcessorFeatures0,
    HvPartitionPropertyCode::RootProcessorFeatures1,
    HvPartitionPropertyCode::RootProcessorXsaveFeatures,
    HvPartitionPropertyCode::RootSyntheticProcFeatures,
    HvPartitionPropertyCode::PrivilegeFlags,
    HvPartitionPropertyCode::SyntheticProcFeatures,
];

/// Properties of the probe partition recorded in the fingerprint.
const PROBE_PROPERTIES: &[HvPartitionPropertyCode] = &[
    HvPartitionPropertyCode::ProcessorFeatures0,
    HvPartitionPropertyCode::ProcessorFeatures1,
    HvPartitionPropertyCode::ProcessorXsaveFeatures,
    HvPartitionPropertyCode::ProcessorClockFrequency,
    HvPartitionPropertyCode::ApicFrequency,
    HvPartitionPropertyCode::PhysicalAddressWidth,
    HvPartitionPropertyCode::XsaveStates,
    HvPartitionPropertyCode::MaxXsaveDataSize,
    HvPartitionPropertyCode::UnimplementedMsrAction,
];

/// The TSC invariant control MSR of the Hyper-V interface, whose intercept
/// probes the MSR-intercept capability.
const HV_X64_MSR_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;

/// The TSC value written to the probe virtual processor. A probe partition
/// starts near zero, so a read-back between this value and twice it shows
/// that the write took effect.
const PROBE_TSC: u64 = 1 << 40;

impl LinuxMshv {
    /// Returns the guest CPU surface and time capabilities that the
    /// hypervisor supports on this host.
    ///
    /// The CPUID table is what the virtual processor of a transient probe
    /// partition reports when the partition enables every processor feature
    /// bank and XSAVE feature of the host partition. The probe partition has
    /// no memory, never runs, and is destroyed before this returns. It also
    /// probes per-VP TSC writes, partition time freezing, and MSR-index
    /// intercepts.
    pub fn cpu_fingerprint(&self) -> Result<BackendFingerprint, Error> {
        let host_property = |code: HvPartitionPropertyCode| {
            self.mshv
                .get_host_partition_property(code.0)
                .map_err(KernelError::from)
        };
        let mut banks = [0; FEATURE_BANKS.len()];
        for (bank, code) in banks.iter_mut().zip(FEATURE_BANKS) {
            *bank = host_property(code)
                .map_err(|error| ErrorInner::GetHostPartitionProperty(code, error))?;
        }
        let [features0, features1, xsave_features] = banks;

        let args = mshv_bindings::mshv_create_partition_v2 {
            pt_flags: 1 << mshv_bindings::MSHV_PT_BIT_LAPIC
                | 1 << mshv_bindings::MSHV_PT_BIT_X2APIC
                | 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES
                | 1 << mshv_bindings::MSHV_PT_BIT_CPU_AND_XSAVE_FEATURES,
            pt_isolation: mshv_bindings::MSHV_PT_ISOLATION_NONE as u64,
            pt_num_cpu_fbanks: mshv_bindings::MSHV_NUM_CPU_FEATURES_BANKS as u16,
            // The banks hold the features to disable.
            pt_cpu_fbanks: [!features0, !features1],
            pt_disabled_xsave: !xsave_features,
            ..Default::default()
        };
        let vmfd = create_vm_with_retry(&self.mshv, &args)?;
        vmfd.initialize()
            .map_err(|e| ErrorInner::CreateVMInitFailed(e.into()))?;
        let vp = vmfd
            .create_vcpu(0)
            .map_err(|e| ErrorInner::CreateVcpu(e.into()))?;

        let cpuid =
            cpu_profile::cpuid::enumerate(|leaf, subleaf| vp.get_cpuid_values(leaf, subleaf, 0, 0))
                .map_err(|e| ErrorInner::FingerprintCpuid(e.into()))?;

        let mut fingerprint = BackendFingerprint::new("mshv", METHOD, cpuid);
        for (code, value) in FEATURE_BANKS.into_iter().zip(banks) {
            fingerprint.set_feature_bank(format!("mshv.host.{code:?}"), value);
        }
        for &code in HOST_PROPERTIES {
            fingerprint.record(&format!("mshv.host.{code:?}"), host_property(code));
        }
        for &code in PROBE_PROPERTIES {
            fingerprint.record(
                &format!("mshv.probe.{code:?}"),
                vmfd.get_partition_property(code.0)
                    .map_err(KernelError::from),
            );
        }

        let probe_value = |name: &str| fingerprint.values.get(name).map(|value| value.0);
        let tsc_frequency_hz = probe_value("mshv.probe.ProcessorClockFrequency");
        let lapic_timer_frequency_hz = probe_value("mshv.probe.ApicFrequency");
        let tsc_offset_control = probe_tsc_write(&vp, &mut fingerprint);
        let time_freeze = probe_time_freeze(&vmfd, &mut fingerprint);
        let msr_index_intercept = probe_msr_index_intercept(&vmfd, &mut fingerprint);

        let time = &mut fingerprint.time;
        time.tsc_frequency_hz = tsc_frequency_hz;
        time.lapic_timer_frequency_hz = lapic_timer_frequency_hz;
        time.tsc_offset_control = Some(tsc_offset_control);
        time.time_freeze = Some(time_freeze);
        // The hypervisor offers no TSC scaling control to the root.
        time.tsc_scaling = None;
        time.msr_intercepts
            .insert("X64MsrIndex".to_owned(), msr_index_intercept);

        drop(vp);
        drop(vmfd);
        Ok(fingerprint)
    }
}

/// Writes the TSC of the probe virtual processor and reads it back, and
/// returns whether the write took effect.
fn probe_tsc_write(vp: &VcpuFd, fingerprint: &mut BackendFingerprint) -> bool {
    let result = (|| {
        vp.set_hvdef_regs(&[HvRegisterAssoc::from((HvX64RegisterName::Tsc, PROBE_TSC))])?;
        let mut registers = [HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0_u64))];
        vp.get_hvdef_regs(&mut registers)?;
        Ok::<_, KernelError>(registers[0].value.as_u64())
    })();
    match result {
        Ok(tsc) => tsc.wrapping_sub(PROBE_TSC) < PROBE_TSC,
        Err(error) => {
            fingerprint.set_unavailable("mshv.probe.tsc_write", &error);
            false
        }
    }
}

/// Freezes and thaws the probe partition's time, and returns whether the
/// hypervisor accepted the freeze. Reading the property back is optional:
/// some hypervisors accept `TimeFreeze` writes but not reads.
fn probe_time_freeze(vmfd: &VmFd, fingerprint: &mut BackendFingerprint) -> bool {
    let code = HvPartitionPropertyCode::TimeFreeze.0;
    if let Err(error) = vmfd.set_partition_property(code, 1) {
        fingerprint.set_unavailable("mshv.probe.TimeFreeze.freeze", &KernelError::from(error));
        return false;
    }
    match vmfd.get_partition_property(code) {
        Ok(value) => fingerprint.set_value("mshv.probe.TimeFreeze.frozen", value),
        Err(error) => {
            fingerprint.set_unavailable("mshv.probe.TimeFreeze.read", &KernelError::from(error))
        }
    }
    if let Err(error) = vmfd.set_partition_property(code, 0) {
        fingerprint.set_unavailable("mshv.probe.TimeFreeze.thaw", &KernelError::from(error));
    }
    true
}

/// Installs an MSR-index intercept on the probe partition, and returns
/// whether the hypervisor accepted it.
fn probe_msr_index_intercept(vmfd: &VmFd, fingerprint: &mut BackendFingerprint) -> bool {
    let result = vmfd.install_intercept(mshv_bindings::mshv_install_intercept {
        access_type_mask: mshv_bindings::HV_INTERCEPT_ACCESS_MASK_READ
            | mshv_bindings::HV_INTERCEPT_ACCESS_MASK_WRITE,
        intercept_type: mshv_bindings::hv_intercept_type_HV_INTERCEPT_TYPE_X64_MSR_INDEX,
        intercept_parameter: mshv_bindings::hv_intercept_parameters {
            msr_index: HV_X64_MSR_TSC_INVARIANT_CONTROL,
        },
    });
    match result {
        Ok(()) => true,
        Err(error) => {
            fingerprint.set_unavailable(
                "mshv.probe.install_intercept.X64MsrIndex",
                &KernelError::from(error),
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::LinuxMshv;
    use cpu_profile::cpuid;
    use test_with_tracing::test;

    #[test]
    #[ignore = "requires /dev/mshv"]
    fn fingerprint_reports_the_supported_surface() {
        let mshv = LinuxMshv::new().unwrap();
        let fingerprint = mshv.cpu_fingerprint().unwrap();
        tracing::info!(unavailable = ?fingerprint.unavailable, "MSHV fingerprint");
        assert_eq!(fingerprint, mshv.cpu_fingerprint().unwrap());
        assert!(cpuid::lookup(&fingerprint.cpuid, 0, 0).is_some());
        assert_eq!(fingerprint.xsave.xcr0_supported.0 & 0x3, 0x3);
        assert_eq!(fingerprint.feature_banks.len(), super::FEATURE_BANKS.len());
        assert!(fingerprint.time.tsc_frequency_hz.is_some());
        assert!(fingerprint.time.lapic_timer_frequency_hz.is_some());
    }
}
