// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the pending NMIs that a restore sets.

use super::native::backend_test;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;

// KVM holds a second NMI that reaches a VP before it has run as a second
// pending NMI, which the activity that a save reads reports as a count, since
// its Boolean cannot hold it. A backend that holds one pending NMI at most
// cannot restore the count, and must refuse it rather than drop the second NMI.
// So a restore either gives the VP the activity that it is given, or fails and
// changes none of it.
backend_test!(restores_or_refuses_two_pending_nmis, requires: []);
fn restores_or_refuses_two_pending_nmis(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.activity()?;

    // The halt shows a restore that changed the activity before it refused.
    let two = vp::Activity {
        mp_state: vp::MpState::Halted,
        nmi_pending: true,
        nmi_pending_count: Some(2),
        ..Default::default()
    };
    if state.set_activity(&two).is_ok() {
        let restored = state.activity()?;
        anyhow::ensure!(
            restored == two,
            "restoring two pending NMIs succeeded, but read back as {restored:?}"
        );
    } else {
        anyhow::ensure!(
            state.activity()? == before,
            "the refused restore of two pending NMIs changed the activity"
        );
    }
    Ok(())
}
