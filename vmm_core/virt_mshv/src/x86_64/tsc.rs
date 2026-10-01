// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The TSC-deadline timer policy of MSHV partitions.
//!
//! MSHV does not reliably deliver TSC-deadline timer events to direct-boot
//! guests, so no partition exposes the timer. The time ABI applies its own
//! feature bank, which hides it too.

/// Returns the processor features (bank 1) to expose: the supported features
/// without the TSC-deadline timer.
fn supported_features1() -> hvdef::HvX64PartitionProcessorFeatures1 {
    super::supported_processor_features1().with_tsc_deadline_tmr_support(false)
}

/// Applies [`supported_features1`] to the partition creation arguments, whose
/// banks hold the disabled features.
pub(super) fn with_features1(
    mut args: mshv_bindings::mshv_create_partition_v2,
) -> mshv_bindings::mshv_create_partition_v2 {
    let mut banks = args.pt_cpu_fbanks;
    banks[1] = !u64::from(supported_features1());
    args.pt_cpu_fbanks = banks;
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86_64::partition_create_args;

    #[test]
    fn partitions_do_not_expose_the_tsc_deadline_timer() {
        let processor_features1 = supported_features1();
        assert!(!processor_features1.tsc_deadline_tmr_support());
        assert!(processor_features1.tsc_adjust_support());
        let args = with_features1(
            partition_create_args(&virt::ProtoPartitionIsolation::None, false, false).unwrap(),
        );
        let pt_cpu_fbanks = args.pt_cpu_fbanks;
        assert_eq!(pt_cpu_fbanks[1], !u64::from(processor_features1));
    }

    /// The feature bank alone hides the TSC-deadline timer in CPUID, so no
    /// CPUID result needs to hide it as well.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn the_feature_bank_hides_the_tsc_deadline_timer_in_cpuid() {
        for (exposed, features1) in [
            (true, super::super::supported_processor_features1()),
            (false, supported_features1()),
        ] {
            let mut args =
                partition_create_args(&virt::ProtoPartitionIsolation::None, false, false).unwrap();
            let mut banks = args.pt_cpu_fbanks;
            banks[1] = !u64::from(features1);
            args.pt_cpu_fbanks = banks;
            let mshv = mshv_ioctls::Mshv::new().unwrap();
            let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
            vmfd.initialize().unwrap();
            let vp = vmfd.create_vcpu(0).unwrap();
            let [_, _, ecx, _] = vp.get_cpuid_values(1, 0, 0, 0).unwrap();
            let tsc_deadline = ecx & (1 << 24) != 0;
            println!("bank 1 tsc_deadline_tmr_support={exposed}: CPUID.1:ECX[24]={tsc_deadline}");
            if !exposed {
                assert!(!tsc_deadline);
            }
        }
    }
}
