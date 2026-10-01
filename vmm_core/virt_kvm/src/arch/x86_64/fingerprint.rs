// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU fingerprint of the KVM backend: the guest CPU surface and time
//! capabilities that KVM supports on this host.

use super::Kvm;
use crate::KvmError;
use cpu_profile::cpuid::CpuidEntry;
use cpu_profile::fingerprint::BackendFingerprint;
use kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
use std::collections::BTreeMap;

const METHOD: &str = "KVM_GET_SUPPORTED_CPUID, without permission for dynamically enabled \
     XSAVE features";

/// KVM's APIC bus cycle before `KVM_CAP_X86_APIC_BUS_CYCLES_NS` made it
/// configurable.
const DEFAULT_APIC_BUS_CYCLE_NS: u64 = 1;

/// The KVM capabilities recorded in the fingerprint.
const CAPABILITIES: &[(&str, u32)] = &[
    ("KVM_CAP_ADJUST_CLOCK", kvm::KVM_CAP_ADJUST_CLOCK),
    ("KVM_CAP_EXT_CPUID", kvm::KVM_CAP_EXT_CPUID),
    ("KVM_CAP_GET_MSR_FEATURES", kvm::KVM_CAP_GET_MSR_FEATURES),
    ("KVM_CAP_GET_TSC_KHZ", kvm::KVM_CAP_GET_TSC_KHZ),
    ("KVM_CAP_HYPERV", kvm::KVM_CAP_HYPERV),
    (
        "KVM_CAP_HYPERV_ENFORCE_CPUID",
        kvm::KVM_CAP_HYPERV_ENFORCE_CPUID,
    ),
    ("KVM_CAP_HYPERV_SYNIC2", kvm::KVM_CAP_HYPERV_SYNIC2),
    ("KVM_CAP_HYPERV_TIME", kvm::KVM_CAP_HYPERV_TIME),
    ("KVM_CAP_KVMCLOCK_CTRL", kvm::KVM_CAP_KVMCLOCK_CTRL),
    ("KVM_CAP_NESTED_STATE", kvm::KVM_CAP_NESTED_STATE),
    ("KVM_CAP_PMU_CAPABILITY", kvm::KVM_CAP_PMU_CAPABILITY),
    ("KVM_CAP_SPLIT_IRQCHIP", kvm::KVM_CAP_SPLIT_IRQCHIP),
    ("KVM_CAP_SYS_ATTRIBUTES", kvm::KVM_CAP_SYS_ATTRIBUTES),
    ("KVM_CAP_TSC_CONTROL", kvm::KVM_CAP_TSC_CONTROL),
    (
        "KVM_CAP_TSC_DEADLINE_TIMER",
        kvm::KVM_CAP_TSC_DEADLINE_TIMER,
    ),
    ("KVM_CAP_VCPU_ATTRIBUTES", kvm::KVM_CAP_VCPU_ATTRIBUTES),
    ("KVM_CAP_VM_TSC_CONTROL", kvm::KVM_CAP_VM_TSC_CONTROL),
    ("KVM_CAP_X2APIC_API", kvm::KVM_CAP_X2APIC_API),
    (
        "KVM_CAP_X86_APIC_BUS_CYCLES_NS",
        kvm::KVM_CAP_X86_APIC_BUS_CYCLES_NS,
    ),
    ("KVM_CAP_X86_DISABLE_EXITS", kvm::KVM_CAP_X86_DISABLE_EXITS),
    ("KVM_CAP_X86_MSR_FILTER", kvm::KVM_CAP_X86_MSR_FILTER),
    ("KVM_CAP_X86_NOTIFY_VMEXIT", kvm::KVM_CAP_X86_NOTIFY_VMEXIT),
    (
        "KVM_CAP_X86_USER_SPACE_MSR",
        kvm::KVM_CAP_X86_USER_SPACE_MSR,
    ),
    ("KVM_CAP_XCRS", kvm::KVM_CAP_XCRS),
    ("KVM_CAP_XSAVE", kvm::KVM_CAP_XSAVE),
    ("KVM_CAP_XSAVE2", kvm::KVM_CAP_XSAVE2),
];

/// The capabilities through which KVM routes guest MSR accesses to the VMM.
const MSR_INTERCEPT_CAPABILITIES: &[&str] =
    &["KVM_CAP_X86_USER_SPACE_MSR", "KVM_CAP_X86_MSR_FILTER"];

fn cpuid_entry(entry: &kvm::kvm_cpuid_entry2) -> CpuidEntry {
    CpuidEntry::new(
        entry.function,
        (entry.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX != 0).then_some(entry.index),
        [entry.eax, entry.ebx, entry.ecx, entry.edx],
    )
}

impl Kvm {
    /// Returns the guest CPU surface and time capabilities that KVM supports
    /// on this host.
    ///
    /// The CPUID table is `KVM_GET_SUPPORTED_CPUID`. Like OpenVMM's own
    /// partitions, the fingerprint does not request permission for
    /// dynamically enabled XSAVE features such as AMX tile data, so KVM omits
    /// them; `kvm.xcomp_guest_supp` records what KVM could support with
    /// permission. The feature MSRs are KVM's MSR-based features. The TSC
    /// rate and the APIC bus cycle come from a transient probe VM without
    /// vCPUs. TSC offset control is `KVM_CAP_VCPU_ATTRIBUTES`, which x86 KVM
    /// reports together with the `KVM_VCPU_TSC_OFFSET` attribute (Linux
    /// 5.16 and later).
    pub fn cpu_fingerprint(&self) -> Result<BackendFingerprint, KvmError> {
        let cpuid = self
            .kvm
            .supported_cpuid()?
            .iter()
            .map(cpuid_entry)
            .collect();
        let mut fingerprint = BackendFingerprint::new("kvm", METHOD, cpuid);

        fingerprint.record(
            "kvm.api_version",
            self.kvm.api_version().map(|version| version as u64),
        );
        let mut capabilities = BTreeMap::new();
        for &(name, capability) in CAPABILITIES {
            if let Some(value) = fingerprint.record(
                &format!("kvm.cap.{name}"),
                self.kvm
                    .check_extension(capability)
                    .map(|value| value as u64),
            ) {
                capabilities.insert(name, value);
            }
        }
        let has_capability = |name| capabilities.get(name).is_some_and(|&value| value != 0);

        match self.kvm.msr_feature_index_list() {
            Ok(indices) => {
                let mut msrs = Vec::with_capacity(indices.len());
                for index in indices {
                    match self.kvm.feature_msr(index) {
                        Ok(value) => msrs.push((index, value)),
                        Err(error) => fingerprint
                            .set_unavailable(format!("kvm.feature_msr.{index:#010x}"), &error),
                    }
                }
                fingerprint.set_feature_msrs(msrs);
            }
            Err(error) => fingerprint.set_unavailable("kvm.msr_feature_index_list", &error),
        }
        match self.kvm.msr_index_list() {
            Ok(indices) => fingerprint.set_supported_msrs(indices),
            Err(error) => fingerprint.set_unavailable("kvm.msr_index_list", &error),
        }
        fingerprint.record("kvm.xcomp_guest_supp", self.kvm.xsave_guest_supported());
        fingerprint.record("kvm.mce_cap_supported", self.kvm.supported_mce_cap());
        match self.kvm.supported_hv_cpuid() {
            Ok(entries) => fingerprint.set_extra_cpuid(
                "kvm.supported_hv_cpuid",
                entries.iter().map(cpuid_entry).collect(),
            ),
            Err(error) => fingerprint.set_unavailable("kvm.supported_hv_cpuid", &error),
        }

        let vm = self.kvm.new_vm(kvm::VmType::Default)?;
        let tsc_frequency_hz = fingerprint.record("kvm.vm.tsc_frequency_hz", vm.tsc_frequency_hz());
        let apic_bus_cycle_ns = fingerprint.record(
            "kvm.vm.cap.KVM_CAP_X86_APIC_BUS_CYCLES_NS",
            vm.check_extension(kvm::KVM_CAP_X86_APIC_BUS_CYCLES_NS)
                .map(|value| value as u64),
        );
        drop(vm);

        let time = &mut fingerprint.time;
        time.tsc_frequency_hz = tsc_frequency_hz;
        // Zero means that the APIC bus cycle is not configurable and has its
        // default length.
        time.lapic_timer_frequency_hz = Some(
            1_000_000_000
                / apic_bus_cycle_ns
                    .filter(|&ns| ns != 0)
                    .unwrap_or(DEFAULT_APIC_BUS_CYCLE_NS),
        );
        // KVM emulates the TSC-deadline timer, but kernels before 6.14 do not
        // report it in the supported CPUID.
        time.tsc_deadline |= has_capability("KVM_CAP_TSC_DEADLINE_TIMER");
        time.tsc_offset_control = Some(has_capability("KVM_CAP_VCPU_ATTRIBUTES"));
        time.tsc_scaling = Some(has_capability("KVM_CAP_TSC_CONTROL"));
        for &name in MSR_INTERCEPT_CAPABILITIES {
            time.msr_intercepts
                .insert(name.to_owned(), has_capability(name));
        }
        Ok(fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::Kvm;
    use cpu_profile::Hex32;
    use cpu_profile::cpuid;
    use test_with_tracing::test;

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn fingerprint_reports_the_supported_surface() {
        let kvm = Kvm::new().unwrap();
        let fingerprint = kvm.cpu_fingerprint().unwrap();
        tracing::info!(unavailable = ?fingerprint.unavailable, "KVM fingerprint");
        assert_eq!(fingerprint, kvm.cpu_fingerprint().unwrap());
        assert!(cpuid::lookup(&fingerprint.cpuid, 0, 0).is_some());
        assert_eq!(fingerprint.xsave.xcr0_supported.0 & 0x3, 0x3);
        assert!(fingerprint.time.tsc_frequency_hz.is_some());
        assert_eq!(
            fingerprint.time.lapic_timer_frequency_hz,
            Some(1_000_000_000)
        );
        assert!(
            fingerprint
                .msrs
                .supported
                .contains(&Hex32(x86defs::X86X_MSR_TSC))
        );
    }
}
