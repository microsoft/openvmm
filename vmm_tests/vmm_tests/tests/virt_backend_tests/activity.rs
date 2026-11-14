// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the activity that a restore sets.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;

// KVM never reports an injected soft exception, #BP or #OF: delivering one
// takes the length of the instruction that raised it, which the ioctl that
// restores the events does not carry, so KVM leaves the guest to raise the
// exception again when it runs that instruction again. The ioctl still accepts
// one, which the VP would deliver with a length that the saved state does not
// give, and its next save would not show. A save never holds one, so the
// restore must refuse one.
backend_test!(refuses_an_injected_soft_exception, kvm_only, requires: []);
fn refuses_an_injected_soft_exception(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.activity()?;

    for vector in [3, 4] {
        // The refused restore changes no activity, the halt included, which a
        // restore that got as far as the MP state would have set.
        let crafted = vp::Activity {
            mp_state: vp::MpState::Halted,
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector,
                error_code: None,
            }),
            ..Default::default()
        };
        anyhow::ensure!(
            state.set_activity(&crafted).is_err(),
            "restoring an injected exception with the vector {vector} succeeded"
        );
        anyhow::ensure!(
            state.activity()? == before,
            "the refused restore of the vector {vector} changed the activity"
        );
    }

    // What a save holds restores: a hard exception, an interrupt, and an NMI.
    for pending_interruption in [
        vp::PendingInterruption::Exception {
            vector: 13,
            error_code: Some(0),
        },
        vp::PendingInterruption::Interrupt { vector: 0x30 },
        vp::PendingInterruption::Nmi,
    ] {
        let activity = vp::Activity {
            pending_interruption: Some(pending_interruption),
            ..Default::default()
        };
        state
            .set_activity(&activity)
            .with_context(|| format!("restoring {activity:?} failed"))?;
        let restored = state.activity()?;
        anyhow::ensure!(
            restored == activity,
            "restoring {activity:?} read back as {restored:?}"
        );
    }
    Ok(())
}
