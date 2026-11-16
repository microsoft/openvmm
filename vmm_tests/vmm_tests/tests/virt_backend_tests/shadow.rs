// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the interrupt shadow that a restore sets.

use super::native::backend_test;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;

// A VP that stops right after an STI that ran with IF clear holds an interrupt
// shadow that, unlike the one that a MOV SS leaves, does not block NMIs at the
// next instruction boundary. The activity that a save reads reports it as the
// STI kind, which KVM on an Intel CPU does. A backend whose interrupt state has
// one bit for both kinds, as MSHV's and WHP's have, cannot restore the kind,
// and must refuse it rather than drop it. KVM on an AMD CPU, whose one kind of
// shadow KVM reports as both, sets it and reads it back without a kind. So a
// restore either gives the VP the activity that it is given, or fails and
// changes none of it, but for the kind on such a CPU.
backend_test!(restores_or_refuses_an_interrupt_shadow_kind, requires: []);
fn restores_or_refuses_an_interrupt_shadow_kind(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let one_kind = partition.caps().vendor.is_amd_compatible();
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.activity()?;

    // The halt shows a restore that changed the activity before it refused.
    let sti = vp::Activity {
        mp_state: vp::MpState::Halted,
        interrupt_shadow: true,
        interrupt_shadow_kind: Some(vp::InterruptShadowKind::Sti),
        ..Default::default()
    };
    if state.set_activity(&sti).is_ok() {
        let restored = state.activity()?;
        let expected = vp::Activity {
            interrupt_shadow_kind: (!one_kind).then_some(vp::InterruptShadowKind::Sti),
            ..sti
        };
        anyhow::ensure!(
            restored == expected,
            "restoring an STI interrupt shadow succeeded, but read back as {restored:?}"
        );
    } else {
        anyhow::ensure!(
            state.activity()? == before,
            "the refused restore of an STI interrupt shadow changed the activity"
        );
    }

    // A kind describes a shadow, so no backend restores one without it, and
    // the refusal changes none of the activity either.
    let before = state.activity()?;
    let kind_only = vp::Activity {
        interrupt_shadow: false,
        ..sti
    };
    anyhow::ensure!(
        state.set_activity(&kind_only).is_err(),
        "restoring a kind without an interrupt shadow succeeded"
    );
    anyhow::ensure!(
        state.activity()? == before,
        "the refused restore of a kind without an interrupt shadow changed the activity"
    );
    Ok(())
}
