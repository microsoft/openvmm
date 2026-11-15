// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the activity that a restore sets.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp;
use virt::vp::AccessVpState as _;

/// Sends the VP, whose APIC ID is 0, `mode`, as a device's MSI does.
fn send(partition: &impl Partition, mode: DeliveryMode) {
    partition.request_msi(Vtl::Vtl0, MsiRequest::new_x86(mode, 0, false, 0, false));
}

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

// A saved state describes a VP outside SMM with no SMI pending, since a save
// refuses a VP in either state, and so does a reset. KVM leaves the SMM state
// as it is when the events are set, so a restore or a reset of a VP that has
// already run must take the VP out of it. An SMI that reaches a stopped VP is
// first requested and only then made pending, when the VP's events are read, as
// a save's check does, and KVM replaces only an SMI that is pending. Before
// Linux 6.17, KVM also refuses the MP state of a VP that waits for a SIPI while
// an SMI is pending.
backend_test!(setting_the_activity_drops_an_smi, kvm_only, requires: []);
fn setting_the_activity_drops_an_smi(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    for made_pending in [false, true] {
        for mp_state in [
            vp::MpState::Running,
            vp::MpState::WaitForSipi,
            vp::MpState::Halted,
        ] {
            send(partition, DeliveryMode::SMI);
            if made_pending {
                anyhow::ensure!(
                    state.check_saveable().is_err(),
                    "the VP does not hold the SMI that was sent to it"
                );
            }

            let activity = vp::Activity {
                mp_state,
                ..Default::default()
            };
            state.set_activity(&activity).with_context(|| {
                format!("restoring {activity:?} over an SMI (made pending: {made_pending}) failed")
            })?;
            state.check_saveable().with_context(|| {
                format!("restoring {activity:?} left the SMI (made pending: {made_pending})")
            })?;
            let restored = state.activity()?;
            anyhow::ensure!(
                restored == activity,
                "restoring {activity:?} read back as {restored:?}"
            );
        }
    }
    Ok(())
}

// A VP outside SMM accepts the INIT that it holds when its MP state is read,
// and the INIT resets the registers that a restore has set. A saved state holds
// no INIT, so a restore must drop the INIT that the VP holds, as the VP would
// have had no INIT when it was saved.
backend_test!(setting_the_activity_drops_a_latched_init, kvm_only, requires: []);
fn setting_the_activity_drops_a_latched_init(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let reset = state.registers()?;
    let restored = vp::Registers {
        rip: 0x5000,
        ..reset
    };
    state.set_registers(&restored)?;

    send(partition, DeliveryMode::INIT);
    let activity = vp::Activity::default();
    state.set_activity(&activity)?;

    // Reading the activity accepts an INIT that the VP still holds.
    let read = state.activity()?;
    anyhow::ensure!(
        read == activity,
        "the activity read back as {read:?}, not as {activity:?}"
    );
    anyhow::ensure!(
        state.registers()? == restored,
        "the VP accepted an INIT that the restore did not drop"
    );
    Ok(())
}

// A restore that refuses the activity changes nothing, as the test above of the
// soft exception requires, and the SMI that the VP holds is part of what the
// restore would otherwise have dropped.
backend_test!(refusing_an_activity_keeps_an_smi, kvm_only, requires: []);
fn refusing_an_activity_keeps_an_smi(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    send(partition, DeliveryMode::SMI);
    for crafted in [
        vp::Activity {
            pending_event: Some(vp::PendingEvent::ExtInt { vector: 0x20 }),
            ..Default::default()
        },
        vp::Activity {
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector: 3,
                error_code: None,
            }),
            ..Default::default()
        },
        vp::Activity {
            mp_state: vp::MpState::Idle,
            ..Default::default()
        },
    ] {
        anyhow::ensure!(
            state.set_activity(&crafted).is_err(),
            "restoring {crafted:?} succeeded"
        );
        anyhow::ensure!(
            state.check_saveable().is_err(),
            "the refused restore of {crafted:?} dropped the SMI"
        );
    }
    Ok(())
}

// A reset sets the activity of a VP that has already run, so it takes the VP
// out of SMM and drops its SMI too.
backend_test!(reset_drops_an_smi, kvm_only, requires: [reset]);
fn reset_drops_an_smi(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    send(partition, DeliveryMode::SMI);
    partition
        .supports_reset()
        .context("selected backend does not support partition reset")?
        .reset()?;
    processor.reset()?;
    processor
        .access_state(Vtl::Vtl0)
        .check_saveable()
        .context("a reset left the SMI that was sent to the VP")?;
    Ok(())
}
