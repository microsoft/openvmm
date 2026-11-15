// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the speculation controls that a save holds.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;

/// The controls of IA32_SPEC_CTRL that a CPUID enumerates: IBRS, STIBP, and
/// SSBD.
const CONTROLS: [u64; 3] = [1 << 0, 1 << 1, 1 << 2];

// A guest whose CPUID enumerates a control of IA32_SPEC_CTRL writes it to
// enable a mitigation, and a restore into a new VP must give the guest the
// value that it saved: the VP starts with 0, so a restore that does not set the
// MSR leaves the mitigation off until the guest next writes it.
backend_test!(spec_ctrl_survives_a_save_and_restore, requires: [spec_ctrl]);
fn spec_ctrl_survives_a_save_and_restore(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(
        state.spec_ctrl()? == vp::SpecCtrl::default(),
        "a new VP does not hold the reset value of IA32_SPEC_CTRL"
    );

    // Enable every control that the host's VP takes: the guest's CPUID
    // enumerates one at least.
    let mut value = 0;
    for control in CONTROLS {
        if state
            .set_spec_ctrl(&vp::SpecCtrl { value: control })
            .is_ok()
        {
            value |= control;
        }
    }
    anyhow::ensure!(
        value != 0,
        "the VP takes none of the controls that its guest's CPUID enumerates"
    );
    let controls = vp::SpecCtrl { value };
    state.set_spec_ctrl(&controls)?;
    state.commit()?;
    anyhow::ensure!(
        state.spec_ctrl()? == controls,
        "the VP does not hold the controls that were just set"
    );

    let saved = mesh::payload::encode(state.save_all()?);

    // The VP then loses them, as a new VP has none.
    state.set_spec_ctrl(&vp::SpecCtrl::default())?;
    state.commit()?;

    state
        .restore_all(&mesh::payload::decode::<vp::VpSavedState>(&saved)?)
        .context("restoring the saved state failed")?;
    let restored = state.spec_ctrl()?;
    anyhow::ensure!(
        restored == controls,
        "the restore did not give the guest its controls: saved {:#x}, restored {:#x}",
        controls.value,
        restored.value
    );
    Ok(())
}
