// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest-writable MSRs that the VP state holds as optional elements beyond
//! the register-backed ones: which of them this backend can save for a
//! partition, and how it reads and writes the three feature and power MSRs and
//! the AMD MSRs that no register name reaches.
//!
//! KVM keeps each of these in the vCPU, where a guest's write lands and a fresh
//! vCPU starts with KVM's own value, so a restore that does not set them
//! drops what the guest wrote.

use crate::KvmError;
use virt::x86::X86PartitionCapabilities;
use virt::x86::vp::MiscMsrs;
use virt::x86::vp::Osvw;
use virt::x86::vp::SvmMsrs;

/// The MSRs of [`MiscMsrs`], in the order of its fields.
const MISC_MSRS: [u32; 3] = [
    x86defs::X86X_IA32_MSR_MISC_ENABLE,
    x86defs::X86X_MSR_MISC_FEATURE_ENABLES,
    x86defs::X86X_MSR_POWER_CTL,
];

/// The MSRs of [`SvmMsrs`], in the order of its fields.
const SVM_MSRS: [u32; 2] = [
    x86defs::X86X_AMD_MSR_VM_CR,
    x86defs::X86X_AMD_MSR_VM_HSAVE_PA,
];

/// The MSRs of [`Osvw`], in the order of its fields.
const OSVW_MSRS: [u32; 2] = [
    x86defs::X86X_AMD_MSR_OSVW_ID_LENGTH,
    x86defs::X86X_AMD_MSR_OSVW_ID_STATUS,
];

/// Returns whether this backend can save and restore every MSR in `msrs` for a
/// partition: the host has each, as `KVM_GET_MSR_INDEX_LIST`
/// (`msr_index_list`) lists them, and the guest is not an SNP guest, whose
/// registers live in an encrypted VMSA that KVM cannot read.
fn saveable(msr_index_list: &[u32], snp: bool, msrs: &[u32]) -> bool {
    !snp && msrs.iter().all(|msr| msr_index_list.contains(msr))
}

/// What the host's KVM keeps for a vCPU beyond the MSRs that its
/// `KVM_GET_MSR_INDEX_LIST` names, as its supported CPUID tells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct KvmSupport {
    /// KVM runs on AMD's SVM, as on an AMD host, whose vendor code keeps a
    /// vCPU's `VM_CR` and `VM_HSAVE_PA`.
    pub svm: bool,
    /// KVM supports OSVW, as on a host that has it, which a vCPU needs for KVM
    /// to keep its OSVW MSRs.
    pub osvw: bool,
}

impl KvmSupport {
    /// Returns what KVM supports, from the result of each leaf and subleaf of
    /// its supported CPUID, `cpuid`, which is zero for a leaf that it lacks.
    pub(super) fn from_supported_cpuid(cpuid: impl Fn(u32, u32) -> [u32; 4]) -> Self {
        let [_, ebx, ecx, edx] = cpuid(0, 0);
        Self {
            svm: x86defs::cpuid::Vendor::from_ebx_ecx_edx(ebx, ecx, edx).is_amd_compatible(),
            osvw: virt::x86::enumerates_osvw(cpuid(0x8000_0001, 0)),
        }
    }
}

/// Sets the capabilities of the MSR state that this backend holds beyond
/// `IA32_SPEC_CTRL`: it clears `umwait_control`, `bndcfgs`, and `osvw`, which
/// `from_cpuid` derived from the guest's CPUID, for a host that lacks the MSR
/// or, for OSVW, whose KVM does not support OSVW, and sets those that no CPUID
/// bit decides, where the host's KVM keeps the MSRs, as `msr_index_list` and
/// `support` say.
pub(super) fn set_capabilities(
    caps: &mut X86PartitionCapabilities,
    msr_index_list: &[u32],
    support: KvmSupport,
    snp: bool,
) {
    caps.umwait_control &= saveable(msr_index_list, snp, &[x86defs::X86X_MSR_UMWAIT_CONTROL]);
    // KVM lists `IA32_BNDCFGS` only where it exposes MPX, which needs the
    // host to enable MPX's state components, as Linux does on a CPU with MPX.
    caps.bndcfgs &= saveable(msr_index_list, snp, &[x86defs::X86X_MSR_BNDCFGS]);
    // `IA32_DEBUGCTL` is an exception to the host's list: KVM virtualizes it
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
    // KVM keeps AMD's OSVW MSRs for a vCPU that has OSVW, and lists neither of
    // them. A vCPU has it where its CPUID enumerates it, which `from_cpuid`
    // records, and KVM supports it: a kernel that keeps a vCPU's features
    // within KVM's own, as recent ones do, refuses the MSRs of any other vCPU.
    caps.osvw &= support.osvw && !snp;
    // KVM on SVM keeps a guest's VM_CR and VM_HSAVE_PA whatever the guest's
    // CPUID says. It lists `VM_HSAVE_PA`, and not `VM_CR`, which it emulates
    // beside it, but it lists `VM_HSAVE_PA` wherever the host can read it, as
    // an Intel host can under a hypervisor that reads it as 0, where KVM has
    // no `VM_CR`, so the capability needs KVM to run on SVM too.
    caps.svm_msrs =
        support.svm && saveable(msr_index_list, snp, &[x86defs::X86X_AMD_MSR_VM_HSAVE_PA]);
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

/// Reads AMD's SVM control and host save area MSRs of `vp`.
pub(super) fn svm_msrs(vp: &kvm::Processor<'_>) -> Result<SvmMsrs, KvmError> {
    let mut values = [0; 2];
    vp.get_msrs(&SVM_MSRS, &mut values)?;
    let [vm_cr, vm_hsave_pa] = values;
    Ok(SvmMsrs { vm_cr, vm_hsave_pa })
}

/// Refuses a `VM_HSAVE_PA` that a guest could not have written: one that is
/// not page-aligned, or that lies beyond the guest's physical address width,
/// `physical_address_width`. KVM checks a guest's write so, but takes any value
/// from the host, and keeps only its page address.
fn check_restorable_vm_hsave_pa(
    vm_hsave_pa: u64,
    physical_address_width: u8,
) -> Result<(), KvmError> {
    let beyond_width = vm_hsave_pa
        .checked_shr(physical_address_width.into())
        .is_some_and(|high| high != 0);
    if vm_hsave_pa & 0xfff != 0 || beyond_width {
        return Err(KvmError::InvalidState(
            "a VM_HSAVE_PA that is not a page address that the guest has",
        ));
    }
    Ok(())
}

/// `VM_CR.SVMDIS`, which makes `EFER.SVME` reserved.
const VM_CR_SVMDIS: u64 = 1 << 4;

/// Refuses a `VM_CR` that sets `SVMDIS` for a vCPU whose `EFER`, `efer`, which
/// the restore sets before `VM_CR`, has `SVME`. No guest holds both, since the
/// CPU refuses either write while the other is set, and KVM refuses the restore
/// only after it has set `SVMDIS` and `LOCK`, which it then keeps for the life
/// of the vCPU.
fn check_restorable_vm_cr(vm_cr: u64, efer: u64) -> Result<(), KvmError> {
    if vm_cr & VM_CR_SVMDIS != 0 && efer & x86defs::X64_EFER_SVME != 0 {
        return Err(KvmError::InvalidState(
            "a VM_CR that sets SVMDIS while EFER.SVME is set",
        ));
    }
    Ok(())
}

/// Writes AMD's SVM control and host save area MSRs of `vp`, whose guest's
/// physical address width is `physical_address_width`. KVM refuses a bit of
/// `VM_CR` outside the five that it emulates, so that MSR goes first, since KVM
/// takes the other from the host, so that a state that fails writes nothing,
/// and a `VM_HSAVE_PA` that the guest could not have written, or a `VM_CR` that
/// KVM would refuse only after it changed it, is refused before either is
/// written.
pub(super) fn set_svm_msrs(
    vp: &kvm::Processor<'_>,
    value: &SvmMsrs,
    physical_address_width: u8,
) -> Result<(), KvmError> {
    let SvmMsrs { vm_cr, vm_hsave_pa } = *value;
    check_restorable_vm_hsave_pa(vm_hsave_pa, physical_address_width)?;
    if vm_cr & VM_CR_SVMDIS != 0 {
        check_restorable_vm_cr(vm_cr, vp.get_sregs()?.efer)?;
    }
    let [vm_cr_msr, vm_hsave_pa_msr] = SVM_MSRS;
    vp.set_msrs(&[(vm_cr_msr, vm_cr), (vm_hsave_pa_msr, vm_hsave_pa)])?;
    Ok(())
}

/// Reads AMD's OSVW MSRs of `vp`.
pub(super) fn osvw(vp: &kvm::Processor<'_>) -> Result<Osvw, KvmError> {
    let mut values = [0; 2];
    vp.get_msrs(&OSVW_MSRS, &mut values)?;
    let [id_length, status] = values;
    Ok(Osvw { id_length, status })
}

/// Writes AMD's OSVW MSRs of `vp`, which KVM takes with any value.
pub(super) fn set_osvw(vp: &kvm::Processor<'_>, value: &Osvw) -> Result<(), KvmError> {
    let Osvw { id_length, status } = *value;
    let [id_length_msr, status_msr] = OSVW_MSRS;
    vp.set_msrs(&[(id_length_msr, id_length), (status_msr, status)])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// Returns the capabilities of a CPU whose CPUID has `waitpkg`, with every
    /// state that no CPUID bit decides left to the backend.
    pub(super) fn caps(waitpkg: bool) -> X86PartitionCapabilities {
        caps_with(waitpkg, false)
    }

    /// Returns the capabilities of a CPU whose CPUID has `waitpkg` and `mpx`.
    fn caps_with(waitpkg: bool, mpx: bool) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [7, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (7, 0) => [0, u32::from(mpx) << 14, u32::from(waitpkg) << 5, 0],
                _ => [0; 4],
            },
        )
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

    /// What KVM supports on an Intel host: neither SVM nor OSVW.
    const INTEL: KvmSupport = KvmSupport {
        svm: false,
        osvw: false,
    };

    /// What KVM supports on an AMD host: SVM and OSVW.
    const AMD: KvmSupport = KvmSupport {
        svm: true,
        osvw: true,
    };

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
        set_capabilities(&mut with, &FULL_LIST, INTEL, false);
        assert_eq!(gates(&with), (true, true, true, true));

        // Without WAITPKG in the guest's CPUID, the guest cannot write
        // IA32_UMWAIT_CONTROL, whatever the host has.
        let mut without_waitpkg = caps(false);
        set_capabilities(&mut without_waitpkg, &FULL_LIST, INTEL, false);
        assert_eq!(gates(&without_waitpkg), (false, true, true, true));
    }

    #[test]
    fn the_host_decides_each_state_by_its_msrs() {
        for (lacked, expected) in [
            (x86defs::X86X_MSR_UMWAIT_CONTROL, (false, true, true, true)),
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
            set_capabilities(&mut caps, &without(lacked), INTEL, false);
            assert_eq!(gates(&caps), expected, "a host without MSR {lacked:#x}");
        }
        // A host with none of the other MSRs saves none of the other states.
        let mut caps = caps(true);
        set_capabilities(&mut caps, &[], INTEL, false);
        assert_eq!(gates(&caps), (false, true, false, false));
    }

    #[test]
    fn an_snp_guest_saves_none_of_them() {
        let mut caps = caps(true);
        set_capabilities(&mut caps, &FULL_LIST, INTEL, true);
        assert_eq!(gates(&caps), (false, false, false, false));
    }

    #[test]
    fn bndcfgs_needs_mpx_in_the_guest_and_the_msr_on_the_host() {
        let with_bndcfgs = [&FULL_LIST[..], &[x86defs::X86X_MSR_BNDCFGS]].concat();
        let bndcfgs = |mpx, list: &[u32], snp| {
            let mut caps = caps_with(false, mpx);
            set_capabilities(&mut caps, list, INTEL, snp);
            caps.bndcfgs
        };
        // A guest with MPX, on a host whose KVM exposes it and so lists the
        // MSR.
        assert!(bndcfgs(true, &with_bndcfgs, false));
        // A host whose KVM does not list it, as one without MPX, holds none.
        assert!(!bndcfgs(true, &FULL_LIST, false));
        // Without MPX in the guest's CPUID, the guest cannot write the MSR.
        assert!(!bndcfgs(false, &with_bndcfgs, false));
        // Nor can KVM read an SNP guest's.
        assert!(!bndcfgs(true, &with_bndcfgs, true));
    }

    /// Returns the capabilities of an AMD CPU whose CPUID has `osvw`.
    fn amd_caps(osvw: bool) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (0x8000_0000, _) => [0x8000_0008, 0, 0, 0],
                (0x8000_0001, _) => [0, 0, u32::from(osvw) << 9, 0],
                _ => [0; 4],
            },
        )
        .unwrap()
    }

    #[test]
    fn osvw_needs_osvw_in_the_guest_and_in_kvm() {
        let osvw = |guest_osvw, kvm_osvw, snp| {
            let mut caps = amd_caps(guest_osvw);
            let support = KvmSupport {
                osvw: kvm_osvw,
                ..AMD
            };
            set_capabilities(&mut caps, &FULL_LIST, support, snp);
            caps.osvw
        };
        // KVM does not list the MSRs, and keeps them for every vCPU whose
        // CPUID enumerates OSVW where it supports OSVW, as on an AMD host.
        assert!(osvw(true, true, false));
        assert!(!osvw(false, true, false));
        // A KVM that does not support OSVW, as on an Intel host, refuses them.
        assert!(!osvw(true, false, false));
        // Nor can KVM read an SNP guest's.
        assert!(!osvw(true, true, true));
    }

    #[test]
    fn the_svm_msrs_need_kvm_on_svm_whatever_the_guest_has() {
        let with_hsave = [&FULL_LIST[..], &[x86defs::X86X_AMD_MSR_VM_HSAVE_PA]].concat();
        let svm_msrs = |caps: X86PartitionCapabilities, list: &[u32], support, snp| {
            let mut caps = caps;
            set_capabilities(&mut caps, list, support, snp);
            caps.svm_msrs
        };
        // KVM on an AMD host, which lists VM_HSAVE_PA, keeps both MSRs, for a
        // guest of either vendor's CPUID, though none of them has SVM.
        assert!(svm_msrs(amd_caps(false), &with_hsave, AMD, false));
        assert!(svm_msrs(caps(false), &with_hsave, AMD, false));
        // KVM on an Intel host keeps neither, even where it lists VM_HSAVE_PA,
        // as it does under a hypervisor that reads the MSR as 0.
        assert!(!svm_msrs(amd_caps(true), &with_hsave, INTEL, false));
        assert!(!svm_msrs(amd_caps(true), &FULL_LIST, INTEL, false));
        // A host that does not list VM_HSAVE_PA has no SVM to keep them.
        assert!(!svm_msrs(amd_caps(true), &FULL_LIST, AMD, false));
        // KVM cannot read an SNP guest's.
        assert!(!svm_msrs(amd_caps(true), &with_hsave, AMD, true));
    }

    #[test]
    fn kvm_support_follows_its_supported_cpuid() {
        let support = |vendor: [u32; 3], ext1_ecx| {
            KvmSupport::from_supported_cpuid(|leaf, _| match leaf {
                0 => [0xd, vendor[0], vendor[1], vendor[2]],
                0x8000_0001 => [0, 0, ext1_ecx, 0],
                _ => [0; 4],
            })
        };
        // "AuthenticAMD" and "GenuineIntel", as EBX, ECX, and EDX order them.
        let amd = [0x6874_7541, 0x444d_4163, 0x6974_6e65];
        let intel = [0x756e_6547, 0x6c65_746e, 0x4965_6e69];
        assert_eq!(support(amd, 1 << 9), AMD);
        assert_eq!(
            support(amd, !(1 << 9)),
            KvmSupport {
                osvw: false,
                ..AMD
            }
        );
        assert_eq!(support(intel, 0), INTEL);
    }

    #[test]
    fn a_restore_refuses_a_vm_hsave_pa_that_a_guest_could_not_write() {
        // A page address within the guest's physical address width.
        check_restorable_vm_hsave_pa(0, 48).unwrap();
        check_restorable_vm_hsave_pa(0x5000, 48).unwrap();
        check_restorable_vm_hsave_pa((1 << 48) - 0x1000, 48).unwrap();
        // Not page-aligned, or beyond the width.
        for value in [0x5001, 0x5800, 1 << 48, !0xfff] {
            assert!(
                check_restorable_vm_hsave_pa(value, 48).is_err(),
                "{value:#x}"
            );
        }
    }

    #[test]
    fn a_restore_refuses_svmdis_while_efer_enables_svm() {
        let svme = x86defs::X64_EFER_SVME;
        // SVMDIS, with or without LOCK, while EFER.SVME is clear, and any
        // other bits while it is set.
        check_restorable_vm_cr(0x10, 0).unwrap();
        check_restorable_vm_cr(0x18, x86defs::X64_EFER_LME).unwrap();
        check_restorable_vm_cr(0x0f, svme).unwrap();
        // SVMDIS while EFER.SVME is set, which no guest can hold.
        assert!(check_restorable_vm_cr(0x10, svme).is_err());
        assert!(check_restorable_vm_cr(0x18, svme | x86defs::X64_EFER_LME).is_err());
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

    /// What the host's KVM supports, from its supported CPUID.
    fn host_support(kvm: &Kvm) -> KvmSupport {
        let supported = kvm.supported_cpuid().unwrap();
        KvmSupport::from_supported_cpuid(|leaf, _| {
            supported
                .iter()
                .find(|entry| entry.function == leaf)
                .map_or([0; 4], |entry| [entry.eax, entry.ebx, entry.ecx, entry.edx])
        })
    }

    /// The host's list has every MSR whose state a partition on it holds, so
    /// the partition's capabilities hold it.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_host_lists_the_msrs_that_its_partitions_hold() {
        let kvm = Kvm::new().unwrap();
        let list = kvm.msr_index_list().unwrap();
        // The guest has no WAITPKG, so no state for IA32_UMWAIT_CONTROL.
        let mut caps = tests::caps(false);
        set_capabilities(&mut caps, &list, host_support(&kvm), false);
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

        // KVM has IA32_DEBUGCTL on every host, IA32_UMWAIT_CONTROL on a host
        // with WAITPKG, and IA32_BNDCFGS on a host with MPX.
        let mut registers = vec![(HvX64RegisterName::DebugCtl, 0x1d9)];
        if list.contains(&0xe1) {
            registers.push((HvX64RegisterName::UmwaitControl, 0xe1));
        }
        if list.contains(&0xd90) {
            registers.push((HvX64RegisterName::Bndcfgs, 0xd90));
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
        if list.contains(&0xd90) {
            // MPX enabled at CPL 0, with BNDPRESERVE, and a bound directory
            // at 0x1000.
            vp.set_msrs(&[(0xd90, 0x1003)]).unwrap();
            let mut value = [0];
            vp.get_msrs(&[0xd90], &mut value).unwrap();
            assert_eq!(value, [0x1003]);
        }
    }

    /// KVM keeps VM_CR and VM_HSAVE_PA exactly where its partitions hold them:
    /// where it runs on SVM and lists VM_HSAVE_PA, whatever the vCPU's CPUID
    /// says. An Intel host whose own read of VM_HSAVE_PA succeeds, as under a
    /// hypervisor that reads it as 0, lists it, but its KVM has no VM_CR. On an
    /// AMD host, a new vCPU holds 0 in each, and the state reads back what it
    /// set; a state that KVM refuses, or whose VM_HSAVE_PA a guest could not
    /// write, writes nothing.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_svm_msrs_are_kept_exactly_where_a_partition_holds_them() {
        let kvm = Kvm::new().unwrap();
        let mut caps = tests::caps(false);
        set_capabilities(
            &mut caps,
            &kvm.msr_index_list().unwrap(),
            host_support(&kvm),
            false,
        );
        let mut vm = kvm.new_vm(VmType::Default).unwrap();
        vm.add_vp(0).unwrap();
        let vp = vm.vp(0);
        assert_eq!(svm_msrs(&vp).is_ok(), caps.svm_msrs);
        if !caps.svm_msrs {
            println!("skipped: KVM does not run on SVM");
            return;
        }

        assert_eq!(svm_msrs(&vp).unwrap(), SvmMsrs::default());
        // DPD and DIS_A20M in VM_CR, and a host save area at 0x5000.
        let value = SvmMsrs {
            vm_cr: 0x5,
            vm_hsave_pa: 0x5000,
        };
        set_svm_msrs(&vp, &value, 48).unwrap();
        assert_eq!(svm_msrs(&vp).unwrap(), value);

        for refused in [
            // A VM_CR bit beyond the five that KVM emulates.
            SvmMsrs {
                vm_cr: 0x20,
                vm_hsave_pa: 0x6000,
            },
            // A VM_HSAVE_PA that is not page-aligned.
            SvmMsrs {
                vm_cr: 0,
                vm_hsave_pa: 0x6001,
            },
        ] {
            assert!(set_svm_msrs(&vp, &refused, 48).is_err(), "{refused:x?}");
            assert_eq!(svm_msrs(&vp).unwrap(), value);
        }
    }

    /// KVM keeps the OSVW MSRs for a vCPU whose CPUID enumerates OSVW, where it
    /// supports OSVW, and the state reads back what it set.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_osvw_msrs_are_kept_for_a_vcpu_with_osvw() {
        let kvm = Kvm::new().unwrap();
        if !host_support(&kvm).osvw {
            println!("skipped: KVM does not support OSVW");
            return;
        }
        let leaf = *kvm
            .supported_cpuid()
            .unwrap()
            .iter()
            .find(|entry| entry.function == 0x8000_0001)
            .unwrap();
        let mut vm = kvm.new_vm(VmType::Default).unwrap();
        vm.add_vp(0).unwrap();
        let vp = vm.vp(0);

        // Without OSVW in its CPUID, the vCPU has neither MSR.
        assert!(osvw(&vp).is_err());
        vp.set_cpuid(&[leaf]).unwrap();
        let value = Osvw {
            id_length: 7,
            status: 5,
        };
        set_osvw(&vp, &value).unwrap();
        assert_eq!(osvw(&vp).unwrap(), value);
    }
}
