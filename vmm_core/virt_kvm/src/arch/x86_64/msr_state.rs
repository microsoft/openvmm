// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest-writable MSRs that the VP state holds as optional elements beyond
//! the register-backed ones: which of them this backend can save for a
//! partition, and how it reads and writes the three feature and power MSRs
//! that no register name reaches.
//!
//! KVM keeps each of these in the vCPU, where a guest's write lands and a fresh
//! vCPU starts with KVM's own value, so a restore that does not set them
//! drops what the guest wrote.

use crate::KvmError;
use virt::x86::X86PartitionCapabilities;
use virt::x86::vp::MiscMsrs;

/// The MSRs of [`MiscMsrs`], in the order of its fields.
const MISC_MSRS: [u32; 3] = [
    x86defs::X86X_IA32_MSR_MISC_ENABLE,
    x86defs::X86X_MSR_MISC_FEATURE_ENABLES,
    x86defs::X86X_MSR_POWER_CTL,
];

/// Returns whether this backend can save and restore every MSR in `msrs` for a
/// partition: the host has each, as `KVM_GET_MSR_INDEX_LIST`
/// (`msr_index_list`) lists them, and the guest is not an SNP guest, whose
/// registers live in an encrypted VMSA that KVM cannot read.
fn saveable(msr_index_list: &[u32], snp: bool, msrs: &[u32]) -> bool {
    !snp && msrs.iter().all(|msr| msr_index_list.contains(msr))
}

/// Sets the capabilities of the MSR state that this backend holds beyond
/// `IA32_SPEC_CTRL`: it clears `umwait_control`, which `from_cpuid` derived
/// from the guest's CPUID, for a host that lacks the MSR, and sets those that
/// no CPUID bit decides, where the host has the MSRs.
pub(super) fn set_capabilities(
    caps: &mut X86PartitionCapabilities,
    msr_index_list: &[u32],
    snp: bool,
) {
    caps.umwait_control &= saveable(msr_index_list, snp, &[x86defs::X86X_MSR_UMWAIT_CONTROL]);
    // `IA32_DEBUGCTL` is the exception to the host's list: KVM virtualizes it
    // for every vCPU, and `KVM_GET_MSRS` and `KVM_SET_MSRS` reach it on every
    // kernel that the time ABI supports, Linux 5.16 and later. A write fails
    // only for a bit that the KVM does not support (on VMX, which drops BTF and
    // LBR instead) or that is above bit 5 (on SVM, which ignores the write
    // without LBR virtualization), so 0 always restores, and so does what a
    // guest holds, unless a newer kernel gave it a bit, as Linux 6.17 gave it
    // RTM_DEBUG. A kernel lists the MSR in `KVM_GET_MSR_INDEX_LIST` only since
    // commit 3700f0788da6 ("KVM: SVM: Add missing save/restore handling of LBR
    // MSRs"), which Linux 7.1 and stable kernels such as 6.18.57 have, so
    // following the list would have a snapshot restore only on such a kernel.
    caps.debug_ctl = !snp;
    caps.misc_msrs = saveable(msr_index_list, snp, &MISC_MSRS);
    // KVM lists `IA32_MCG_STATUS`, which it emulates for every vCPU, but not
    // the registers of the banks, whose number `IA32_MCG_CAP` gives for each
    // vCPU, so the capability follows `IA32_MCG_STATUS`.
    caps.machine_check = saveable(msr_index_list, snp, &[x86defs::X86X_MSR_MCG_STATUS]);
}

/// Reads the feature and power MSRs of `vp`.
pub(super) fn misc_msrs(vp: &kvm::Processor<'_>) -> Result<MiscMsrs, KvmError> {
    let mut values = [0; 3];
    vp.get_msrs(&MISC_MSRS, &mut values)?;
    let [misc_enable, misc_features_enables, power_ctl] = values;
    Ok(MiscMsrs {
        misc_enable,
        misc_features_enables,
        power_ctl,
    })
}

/// Writes the feature and power MSRs of `vp`. KVM refuses a reserved bit of
/// `MSR_MISC_FEATURES_ENABLES`, so a state that sets one fails the restore, and
/// that MSR goes first, since KVM takes any value of the others, so that a
/// state that fails writes nothing.
pub(super) fn set_misc_msrs(vp: &kvm::Processor<'_>, value: &MiscMsrs) -> Result<(), KvmError> {
    let MiscMsrs {
        misc_enable,
        misc_features_enables,
        power_ctl,
    } = *value;
    let [misc_enable_msr, misc_features_enables_msr, power_ctl_msr] = MISC_MSRS;
    vp.set_msrs(&[
        (misc_features_enables_msr, misc_features_enables),
        (misc_enable_msr, misc_enable),
        (power_ctl_msr, power_ctl),
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// Returns the capabilities of a CPU whose CPUID has `waitpkg`, with every
    /// state that no CPUID bit decides left to the backend.
    pub(super) fn caps(waitpkg: bool) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(&topology, &mut |leaf, subleaf| {
            match (leaf, subleaf) {
                (0, _) => [7, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (7, 0) => [0, 0, u32::from(waitpkg) << 5, 0],
                _ => [0; 4],
            }
        })
        .unwrap()
    }

    /// `KVM_GET_MSR_INDEX_LIST` of a host that has every MSR of the state.
    const FULL_LIST: [u32; 8] = [
        x86defs::X86X_MSR_UMWAIT_CONTROL,
        x86defs::X86X_IA32_MSR_MISC_ENABLE,
        x86defs::X86X_MSR_MISC_FEATURE_ENABLES,
        x86defs::X86X_MSR_MCG_STATUS,
        x86defs::X86X_MSR_MCG_CTL,
        x86defs::X86X_MSR_DEBUGCTL,
        x86defs::X86X_MSR_POWER_CTL,
        x86defs::X86X_MSR_TSC,
    ];

    fn without(msr: u32) -> Vec<u32> {
        FULL_LIST.into_iter().filter(|&m| m != msr).collect()
    }

    /// The capabilities as `(umwait_control, debug_ctl, misc_msrs,
    /// machine_check)`.
    fn gates(caps: &X86PartitionCapabilities) -> (bool, bool, bool, bool) {
        (
            caps.umwait_control,
            caps.debug_ctl,
            caps.misc_msrs,
            caps.machine_check,
        )
    }

    #[test]
    fn a_host_that_has_the_msrs_saves_what_the_guest_can_write() {
        let mut with = caps(true);
        set_capabilities(&mut with, &FULL_LIST, false);
        assert_eq!(gates(&with), (true, true, true, true));

        // Without WAITPKG in the guest's CPUID, the guest cannot write
        // IA32_UMWAIT_CONTROL, whatever the host has.
        let mut without_waitpkg = caps(false);
        set_capabilities(&mut without_waitpkg, &FULL_LIST, false);
        assert_eq!(gates(&without_waitpkg), (false, true, true, true));
    }

    #[test]
    fn the_host_decides_each_state_by_its_msrs() {
        for (lacked, expected) in [
            (
                x86defs::X86X_MSR_UMWAIT_CONTROL,
                (false, true, true, true),
            ),
            // One of the three feature and power MSRs takes the state.
            (
                x86defs::X86X_IA32_MSR_MISC_ENABLE,
                (true, true, false, true),
            ),
            (
                x86defs::X86X_MSR_MISC_FEATURE_ENABLES,
                (true, true, false, true),
            ),
            (x86defs::X86X_MSR_POWER_CTL, (true, true, false, true)),
            // IA32_MCG_STATUS is the machine-check state's anchor, and
            // IA32_MCG_CTL is not.
            (x86defs::X86X_MSR_MCG_STATUS, (true, true, true, false)),
            (x86defs::X86X_MSR_MCG_CTL, (true, true, true, true)),
            // A kernel that lacks the fix that lists IA32_DEBUGCTL still has
            // the MSR, so the host's list does not decide it.
            (x86defs::X86X_MSR_DEBUGCTL, (true, true, true, true)),
        ] {
            let mut caps = caps(true);
            set_capabilities(&mut caps, &without(lacked), false);
            assert_eq!(gates(&caps), expected, "a host without MSR {lacked:#x}");
        }
        // A host with none of the other MSRs saves none of the other states.
        let mut caps = caps(true);
        set_capabilities(&mut caps, &[], false);
        assert_eq!(gates(&caps), (false, true, false, false));
    }

    #[test]
    fn an_snp_guest_saves_none_of_them() {
        let mut caps = caps(true);
        set_capabilities(&mut caps, &FULL_LIST, true);
        assert_eq!(gates(&caps), (false, false, false, false));
    }
}

/// Tests against the host's KVM.
#[cfg(test)]
mod kvm_tests {
    use super::*;
    use hvdef::HvX64RegisterName;
    use kvm::Kvm;
    use kvm::VmType;
    use test_with_tracing::test;

    /// The feature and power MSRs that the state sets are the ones of its
    /// fields, and the state reads them back from the same MSRs.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_misc_msrs_are_the_registers_that_their_fields_name() {
        let kvm = Kvm::new().unwrap();
        let mut vm = kvm.new_vm(VmType::Default).unwrap();
        vm.add_vp(0).unwrap();
        let vp = vm.vp(0);

        let value = MiscMsrs {
            misc_enable: 0x85_1a09,
            // The CPUID-faulting bit, the only one that KVM takes.
            misc_features_enables: 1,
            power_ctl: 0x8_0002,
        };
        set_misc_msrs(&vp, &value).unwrap();
        let mut msrs = [0; 3];
        vp.get_msrs(
            &[
                x86defs::X86X_IA32_MSR_MISC_ENABLE,
                x86defs::X86X_MSR_MISC_FEATURE_ENABLES,
                x86defs::X86X_MSR_POWER_CTL,
            ],
            &mut msrs,
        )
        .unwrap();
        assert_eq!(msrs, [0x85_1a09, 1, 0x8_0002]);
        assert_eq!(misc_msrs(&vp).unwrap(), value);

        // KVM refuses a reserved bit of the features, so a state that sets
        // one fails the restore, and writes nothing.
        assert!(
            set_misc_msrs(
                &vp,
                &MiscMsrs {
                    misc_enable: 0x851_809,
                    misc_features_enables: 2,
                    power_ctl: 0,
                },
            )
            .is_err()
        );
        assert_eq!(misc_msrs(&vp).unwrap(), value);
    }

    /// The host's list has every MSR whose state a partition on it holds, so
    /// the partition's capabilities hold it.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_host_lists_the_msrs_that_its_partitions_hold() {
        let list = Kvm::new().unwrap().msr_index_list().unwrap();
        // The guest has no WAITPKG, so no state for IA32_UMWAIT_CONTROL.
        let mut caps = tests::caps(false);
        set_capabilities(&mut caps, &list, false);
        assert!(caps.debug_ctl && caps.misc_msrs && caps.machine_check);
        assert!(!caps.umwait_control);
    }

    /// KVM has the MSRs that the register names of the state stand for, and a
    /// new vCPU reads them as 0.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_register_names_of_the_state_are_msrs_that_kvm_has() {
        let kvm = Kvm::new().unwrap();
        let list = kvm.msr_index_list().unwrap();
        let mut vm = kvm.new_vm(VmType::Default).unwrap();
        vm.add_vp(0).unwrap();
        let vp = vm.vp(0);

        // KVM has IA32_DEBUGCTL on every host, and IA32_UMWAIT_CONTROL on a
        // host with WAITPKG.
        let mut registers = vec![(HvX64RegisterName::DebugCtl, 0x1d9)];
        if list.contains(&0xe1) {
            registers.push((HvX64RegisterName::UmwaitControl, 0xe1));
        }
        for (name, msr) in registers {
            assert_eq!(super::super::regs::register_to_msr(name).unwrap(), msr);
            let mut value = [!0];
            vp.get_msrs(&[msr], &mut value).unwrap();
            assert_eq!(value, [0], "MSR {msr:#x}");
        }
        if list.contains(&0xe1) {
            // The wait limit in bits 31:2, and C0.2 disabled in bit 0.
            vp.set_msrs(&[(0xe1, 0x1005)]).unwrap();
            let mut value = [0];
            vp.get_msrs(&[0xe1], &mut value).unwrap();
            assert_eq!(value, [0x1005]);
        }
    }
}
