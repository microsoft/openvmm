// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the guest-writable MSRs that a save holds beyond
//! the registers: the feature and power controls, `IA32_UMWAIT_CONTROL`, and
//! `IA32_DEBUGCTL`.
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
