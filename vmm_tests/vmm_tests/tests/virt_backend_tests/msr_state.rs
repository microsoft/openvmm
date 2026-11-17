// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the guest-writable MSRs that a save holds beyond
//! the registers: the feature and power controls, `IA32_UMWAIT_CONTROL`,
//! `IA32_BNDCFGS`, `IA32_DEBUGCTL`, the machine-check state, and AMD's SVM and
//! OSVW MSRs.
//!
//! A guest writes these MSRs at boot, a new VP starts without what it wrote,
//! and a restore into a new VP must give the guest back what it saved.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState;

/// Saves the VP's state as a snapshot holds it.
fn save(state: &mut impl AccessVpState) -> anyhow::Result<Vec<u8>> {
    Ok(mesh::payload::encode(state.save_all()?))
}

/// Restores the VP's state from a snapshot.
fn restore(state: &mut impl AccessVpState, saved: &[u8]) -> anyhow::Result<()> {
    state
        .restore_all(&mesh::payload::decode::<vp::VpSavedState>(saved)?)
        .context("restoring the saved state failed")
}

backend_test!(misc_msrs_survive_a_save_and_restore, requires: [misc_msrs]);
fn misc_msrs_survive_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);

    // A new VP holds the values that the Microsoft hypervisor resets them to.
    let new_vp = vp::MiscMsrs {
        misc_enable: x86defs::MISC_ENABLE_DEFAULT.into(),
        misc_features_enables: 0,
        power_ctl: 0,
    };
    anyhow::ensure!(
        state.misc_msrs()? == new_vp,
        "a new VP does not hold the reset values: {:#x?}",
        state.misc_msrs()?
    );

    // The guest turns off fast strings and the hardware prefetcher, faults
    // CPUID, and writes the power control.
    let written = vp::MiscMsrs {
        misc_enable: u64::from(x86defs::MISC_ENABLE_DEFAULT) ^ 0x201,
        misc_features_enables: 1,
        power_ctl: 0x8_0002,
    };
    state.set_misc_msrs(&written)?;
    state.commit()?;
    anyhow::ensure!(
        state.misc_msrs()? == written,
        "the VP does not hold the values that were just set: {:#x?}",
        state.misc_msrs()?
    );

    let saved = save(&mut state)?;

    // The VP then loses them, as a new VP has none.
    state.set_misc_msrs(&new_vp)?;
    state.commit()?;
    anyhow::ensure!(state.misc_msrs()? == new_vp, "the VP keeps the values");

    restore(&mut state, &saved)?;
    let restored = state.misc_msrs()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its values: saved {written:#x?}, restored {restored:#x?}"
    );
    Ok(())
}

// A VM reset, which a guest's reboot requests, resets each VP's state, and what
// the guest wrote must not outlive it.
backend_test!(a_reset_returns_the_feature_msrs_to_what_a_new_vp_holds, requires: [misc_msrs]);
fn a_reset_returns_the_feature_msrs_to_what_a_new_vp_holds(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let new_vp = processor.access_state(Vtl::Vtl0).misc_msrs()?;
    {
        let mut state = processor.access_state(Vtl::Vtl0);
        state.set_misc_msrs(&vp::MiscMsrs {
            misc_enable: new_vp.misc_enable ^ 0x201,
            misc_features_enables: 1,
            power_ctl: 0x8_0002,
        })?;
        state.commit()?;
    }

    processor.reset()?;

    let reset = processor.access_state(Vtl::Vtl0).misc_msrs()?;
    anyhow::ensure!(
        reset == new_vp,
        "the reset left the VP with the guest's values: new {new_vp:#x?}, reset {reset:#x?}"
    );
    Ok(())
}

// A guest whose CPUID enumerates WAITPKG sets how long `UMWAIT` and `TPAUSE` may
// wait: Linux writes the MSR on every CPU at boot.
backend_test!(umwait_control_survives_a_save_and_restore, requires: [umwait_control]);
fn umwait_control_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(
        state.umwait_control()? == vp::UmwaitControl::default(),
        "a new VP does not hold the reset value of IA32_UMWAIT_CONTROL"
    );

    // The longest wait in bits 31:2, and C0.2 disabled in bit 0.
    let written = vp::UmwaitControl { value: 0x1005 };
    state.set_umwait_control(&written)?;
    state.commit()?;
    anyhow::ensure!(
        state.umwait_control()? == written,
        "the VP does not hold the value that was just set"
    );

    let saved = save(&mut state)?;
    state.set_umwait_control(&vp::UmwaitControl::default())?;
    state.commit()?;

    restore(&mut state, &saved)?;
    let restored = state.umwait_control()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its control: saved {:#x}, restored {:#x}",
        written.value,
        restored.value
    );
    Ok(())
}

/// The controls of IA32_DEBUGCTL that a backend may keep, which are LBR, BTF,
/// BUS_LOCK_DETECT, FREEZE_LBRS_ON_PMI, and RTM_DEBUG.
const DEBUG_CONTROLS: [u64; 5] = [1 << 0, 1 << 1, 1 << 2, 1 << 11, 1 << 15];

// A guest whose CPUID enumerates MPX can enable it at CPL 0 through
// IA32_BNDCFGS, which a new VP holds as 0.
backend_test!(bndcfgs_survives_a_save_and_restore, requires: [bndcfgs]);
fn bndcfgs_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(
        state.bndcfgs()? == vp::Bndcfgs::default(),
        "a new VP does not hold the reset value of IA32_BNDCFGS"
    );

    // MPX enabled, with BNDPRESERVE, and a bound directory at 0x1000.
    let written = vp::Bndcfgs { value: 0x1003 };
    state.set_bndcfgs(&written)?;
    state.commit()?;
    anyhow::ensure!(
        state.bndcfgs()? == written,
        "the VP does not hold the value that was just set"
    );

    let saved = save(&mut state)?;
    state.set_bndcfgs(&vp::Bndcfgs::default())?;
    state.commit()?;

    restore(&mut state, &saved)?;
    let restored = state.bndcfgs()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its configuration: saved {:#x}, restored {:#x}",
        written.value,
        restored.value
    );
    Ok(())
}

// KVM on an AMD host keeps a guest's VM_CR and VM_HSAVE_PA, though the guest's
// CPUID has no SVM, and a new VP holds 0 in each.
backend_test!(svm_msrs_survive_a_save_and_restore, requires: [svm_msrs]);
fn svm_msrs_survive_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(
        state.svm_msrs()? == vp::SvmMsrs::default(),
        "a new VP does not hold the reset values of VM_CR and VM_HSAVE_PA: {:#x?}",
        state.svm_msrs()?
    );

    // DPD and DIS_A20M in VM_CR, and a host save area at 0x5000. SVMDIS would
    // stay set for the life of the vCPU, as KVM keeps it once set.
    let written = vp::SvmMsrs {
        vm_cr: 0x5,
        vm_hsave_pa: 0x5000,
    };
    state.set_svm_msrs(&written)?;
    state.commit()?;
    anyhow::ensure!(
        state.svm_msrs()? == written,
        "the VP does not hold the values that were just set"
    );

    let saved = save(&mut state)?;
    state.set_svm_msrs(&vp::SvmMsrs::default())?;
    state.commit()?;

    // A host save area that is not page-aligned, which a guest cannot write,
    // is refused, and writes nothing.
    anyhow::ensure!(
        state
            .set_svm_msrs(&vp::SvmMsrs {
                vm_cr: 0x1,
                vm_hsave_pa: 0x5008,
            })
            .is_err(),
        "a VM_HSAVE_PA that is not page-aligned is restored"
    );
    anyhow::ensure!(
        state.svm_msrs()? == vp::SvmMsrs::default(),
        "a refused state changed the VP: {:#x?}",
        state.svm_msrs()?
    );

    restore(&mut state, &saved)?;
    let restored = state.svm_msrs()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its values: saved {written:#x?}, restored {restored:#x?}"
    );
    Ok(())
}

// A guest whose CPUID enumerates OSVW can write AMD's OSVW MSRs, whose values a
// new VP takes from the backend: KVM derives them from the host's.
backend_test!(osvw_survives_a_save_and_restore, requires: [osvw]);
fn osvw_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let new_vp = state.osvw()?;

    let written = vp::Osvw {
        id_length: new_vp.id_length.wrapping_add(4),
        status: new_vp.status ^ 0x5,
    };
    state.set_osvw(&written)?;
    state.commit()?;
    anyhow::ensure!(
        state.osvw()? == written,
        "the VP does not hold the values that were just set"
    );

    let saved = save(&mut state)?;
    state.set_osvw(&new_vp)?;
    state.commit()?;

    restore(&mut state, &saved)?;
    let restored = state.osvw()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its values: saved {written:#x?}, restored {restored:#x?}"
    );
    Ok(())
}

// A VM reset puts back the OSVW values that a new VP holds, as a reboot's
// firmware would write them again, not the reset value of the MSRs, 0.
backend_test!(a_reset_returns_osvw_to_what_a_new_vp_holds, requires: [osvw]);
fn a_reset_returns_osvw_to_what_a_new_vp_holds(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let new_vp = processor.access_state(Vtl::Vtl0).osvw()?;
    {
        let mut state = processor.access_state(Vtl::Vtl0);
        state.set_osvw(&vp::Osvw {
            id_length: new_vp.id_length.wrapping_add(4),
            status: new_vp.status ^ 0x5,
        })?;
        state.commit()?;
    }

    processor.reset()?;

    let reset = processor.access_state(Vtl::Vtl0).osvw()?;
    anyhow::ensure!(
        reset == new_vp,
        "the reset did not give the VP the values of a new VP: new {new_vp:#x?}, reset {reset:#x?}"
    );
    Ok(())
}

// A backend drops the controls that its host or its guest's CPUID does not
// enumerate, so the test saves the controls that the VP keeps. Linux sets
// BUS_LOCK_DETECT when the guest's CPUID has it.
backend_test!(debug_ctl_survives_a_save_and_restore, requires: [debug_ctl]);
fn debug_ctl_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(
        state.debug_ctl()? == vp::DebugCtl::default(),
        "a new VP does not hold the reset value of IA32_DEBUGCTL"
    );

    let mut value = 0;
    for control in DEBUG_CONTROLS {
        let candidate = vp::DebugCtl {
            value: value | control,
        };
        if state.set_debug_ctl(&candidate).is_ok() && state.debug_ctl()? == candidate {
            value |= control;
        }
    }
    let written = vp::DebugCtl { value };
    state.set_debug_ctl(&written)?;
    state.commit()?;
    if value == 0 {
        // A VP of a nested or a virtualized host keeps none, and the round
        // trip below then only shows that a state without a control restores.
        tracing::warn!("the VP keeps none of the controls of IA32_DEBUGCTL");
    }
    anyhow::ensure!(
        state.debug_ctl()? == written,
        "the VP does not hold the controls that were just set"
    );

    let saved = save(&mut state)?;
    state.set_debug_ctl(&vp::DebugCtl::default())?;
    state.commit()?;

    restore(&mut state, &saved)?;
    let restored = state.debug_ctl()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its controls: saved {:#x}, restored {:#x}",
        written.value,
        restored.value
    );
    Ok(())
}

// A guest with machine-check support (`CONFIG_X86_MCE` in Linux) enables the
// errors of its banks and their CMCI, and clears their status at boot, and the
// registers that it writes live in the VP.
backend_test!(machine_check_state_survives_a_save_and_restore, requires: [machine_check]);
fn machine_check_state_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);

    // A new VP records no error. Whether a bank enables every error depends on
    // the host, so the test takes what the VP holds as the new state.
    let new_vp = state.machine_check()?;
    anyhow::ensure!(
        new_vp.banks.len() >= 2,
        "a new VP has fewer than two banks: {}",
        new_vp.banks.len()
    );
    anyhow::ensure!(
        new_vp.status == 0
            && new_vp
                .banks
                .iter()
                .all(|bank| (bank.status, bank.addr, bank.misc, bank.ctl2) == (0, 0, 0, 0)),
        "a new VP records an error: {new_vp:#x?}"
    );

    let mut written = new_vp.clone();
    written.status = 0x7;
    // A control is 0 or all ones: a new VP holds one, so the other is a change
    // on either vendor's host.
    written.banks[0].ctl = 0;
    written.banks[0].addr = 0x1234;
    written.banks[0].misc = 0x5678;
    written.banks[1].ctl = !0;
    // CMCI enabled, with a threshold.
    written.banks[1].ctl2 = 0x4000_0003;
    let last = written.banks.len() - 1;
    written.banks[last].addr = 0xdead_beef;
    written.banks[last].status = 0x8000_0000_0000_0001;
    if state.set_machine_check(&written).is_err() {
        // The VP has no CMCI, so it refuses a nonzero control.
        written.banks[1].ctl2 = 0;
        state.set_machine_check(&written)?;
    }
    state.commit()?;
    let held = state.machine_check()?;
    anyhow::ensure!(
        held == written,
        "the VP does not hold the state that was just set: {held:#x?}"
    );

    let saved = save(&mut state)?;

    // The state at reset resets every bank, which leaves what a new VP held.
    state.set_machine_check(&vp::MachineCheck::default())?;
    state.commit()?;
    anyhow::ensure!(
        state.machine_check()? == new_vp,
        "the state at reset does not leave the state of a new VP: {:#x?}",
        state.machine_check()?
    );

    // A state that lists more banks than the VP has is refused, and writes
    // nothing.
    let mut too_many = written.clone();
    too_many.banks.push(Default::default());
    anyhow::ensure!(
        state.set_machine_check(&too_many).is_err(),
        "a state with more banks than the VP has is restored"
    );
    anyhow::ensure!(
        state.machine_check()? == new_vp,
        "a refused state changed the VP: {:#x?}",
        state.machine_check()?
    );

    restore(&mut state, &saved)?;
    let restored = state.machine_check()?;
    anyhow::ensure!(
        restored == written,
        "the restore did not give the guest its state: saved {written:#x?}, restored {restored:#x?}"
    );
    Ok(())
}

// A VM reset resets the banks that the guest wrote, to what a new VP holds.
backend_test!(a_reset_returns_the_machine_check_state_to_what_a_new_vp_holds, requires: [machine_check]);
fn a_reset_returns_the_machine_check_state_to_what_a_new_vp_holds(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let new_vp = processor.access_state(Vtl::Vtl0).machine_check()?;
    anyhow::ensure!(
        new_vp.banks.len() >= 2,
        "a new VP has fewer than two banks: {}",
        new_vp.banks.len()
    );
    {
        let mut state = processor.access_state(Vtl::Vtl0);
        let mut written = new_vp.clone();
        written.status = 0x7;
        written.banks[0].addr = 0x1234;
        // A control is 0 or all ones, so this is the one that a new VP lacks.
        written.banks[1].ctl = !new_vp.banks[1].ctl;
        state.set_machine_check(&written)?;
        state.commit()?;
    }

    processor.reset()?;

    let reset = processor.access_state(Vtl::Vtl0).machine_check()?;
    anyhow::ensure!(
        reset == new_vp,
        "the reset left the VP with the guest's state: new {new_vp:#x?}, reset {reset:#x?}"
    );
    Ok(())
}
