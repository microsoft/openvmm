// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the activity that a restore sets.

use super::fixture::Fixture;
use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::BindProcessor as _;
use virt::Hypervisor;
use virt::Partition;
use virt::PartitionMemoryMapper;
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

// KVM_SET_VCPU_EVENTS accepts any count of pending NMIs, but KVM cuts the count
// down to what it holds: one while an NMI is blocked or being injected, and
// otherwise two. A restore of a count that KVM would cut down would lose an NMI
// that the saved state holds, which the VP's next save would not show. A save
// holds a count only for two pending NMIs, so only a damaged or crafted saved
// state holds any other, and the restore must refuse it.
backend_test!(refuses_a_pending_nmi_count_that_kvm_would_not_hold, kvm_only, requires: []);
fn refuses_a_pending_nmi_count_that_kvm_would_not_hold(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.activity()?;

    // The halt shows a restore that changed the activity before it refused.
    let two = || vp::Activity {
        mp_state: vp::MpState::Halted,
        nmi_pending: true,
        nmi_pending_count: Some(2),
        ..Default::default()
    };
    for (what, crafted) in [
        (
            "two pending NMIs with NMIs blocked",
            vp::Activity {
                nmi_masked: true,
                ..two()
            },
        ),
        (
            "two pending NMIs with an NMI being injected",
            vp::Activity {
                pending_interruption: Some(vp::PendingInterruption::Nmi),
                ..two()
            },
        ),
        (
            "a count of two with no NMI pending",
            vp::Activity {
                nmi_pending: false,
                ..two()
            },
        ),
        (
            "a count of one",
            vp::Activity {
                nmi_pending_count: Some(1),
                ..two()
            },
        ),
        (
            "a count of three",
            vp::Activity {
                nmi_pending_count: Some(3),
                ..two()
            },
        ),
    ] {
        anyhow::ensure!(
            state.set_activity(&crafted).is_err(),
            "restoring {what} succeeded"
        );
        anyhow::ensure!(
            state.activity()? == before,
            "the refused restore of {what} changed the activity"
        );
    }

    // What a save holds restores: two pending NMIs behind an interrupt that is
    // being injected, and an interrupt shadow, which do not block NMIs.
    let behind_an_interrupt = vp::Activity {
        pending_interruption: Some(vp::PendingInterruption::Interrupt { vector: 0x30 }),
        interrupt_shadow: true,
        ..two()
    };
    state
        .set_activity(&behind_an_interrupt)
        .with_context(|| format!("restoring {behind_an_interrupt:?} failed"))?;
    let restored = state.activity()?;
    anyhow::ensure!(
        restored == behind_an_interrupt,
        "restoring {behind_an_interrupt:?} read back as {restored:?}"
    );
    Ok(())
}

// KVM holds a second NMI that reaches a VP before it has run as a second
// pending NMI: it injects one NMI at once, and the other once the guest's
// handler for the first returns. The saved state must hold both, and a restore
// into a new VP must give them back, or the guest loses an NMI. A new partition
// supplies the VP that restores them, because a VP that already holds the NMIs
// could not show that the restore wrote them.
fn restores_two_pending_nmis<H: Hypervisor>(fixture: &Fixture, hv: &mut H) -> anyhow::Result<()>
where
    H::Partition: Partition + PartitionMemoryMapper,
{
    let (saved, activity) = {
        let (partition, mut binder) = fixture.build(hv, false)?;
        let mut processor = binder.bind()?;
        send(&partition, DeliveryMode::NMI);
        send(&partition, DeliveryMode::NMI);
        let mut state = processor.access_state(Vtl::Vtl0);
        let activity = state.activity()?;
        (mesh::payload::encode(state.save_all()?), activity)
    };
    anyhow::ensure!(
        activity.nmi_pending && activity.nmi_pending_count == Some(2),
        "the VP that two NMIs reached does not hold them: {activity:?}"
    );

    let (_partition, mut binder) = fixture.build(hv, false)?;
    let mut processor = binder.bind()?;
    let mut state = processor.access_state(Vtl::Vtl0);
    anyhow::ensure!(!state.activity()?.nmi_pending, "a new VP holds an NMI");
    state
        .restore_all(&mesh::payload::decode::<vp::VpSavedState>(&saved)?)
        .context("restoring the saved state failed")?;
    let restored = state.activity()?;
    anyhow::ensure!(
        restored == activity,
        "the restore did not give the VP its NMIs: saved {activity:?}, restored {restored:?}"
    );
    Ok(())
}

// This contract builds a second partition, so it registers by hand, with what
// `backend_test!` registers for KVM.
petri::multitest!(vec![crate::native::kvm::test(
    "kvm::restores_two_pending_nmis",
    false,
    || {
        // Evaluate the capabilities, which the test reads, as `backend_test!`
        // does, so that a failed probe fails the test.
        let _ = crate::native::kvm::capabilities(false);
        true
    },
    async |driver, mut hv| restores_two_pending_nmis(&Fixture::new(driver).await?, &mut hv),
)]);

// Without KVM_CAP_EXCEPTION_PAYLOAD, which OpenVMM does not enable,
// KVM_SET_VCPU_EVENTS silently drops an exception that it is given as pending,
// and an exception that it has not injected yet can only be restored as
// injected. So the restore must refuse a pending exception that would be lost,
// and leave the activity as it was.
backend_test!(refuses_a_pending_exception_that_kvm_would_drop, kvm_only, requires: []);
fn refuses_a_pending_exception_that_kvm_would_drop(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.activity()?;

    let pending = |vector, error_code, parameter| vp::PendingEvent::Exception {
        vector,
        error_code,
        parameter,
    };
    let injected = |vector, error_code| vp::PendingInterruption::Exception { vector, error_code };
    for (pending_event, pending_interruption) in [
        // Alone, or beside an interruption that is not the same exception.
        (pending(13, Some(0), 0), None),
        (pending(13, Some(0), 0), Some(injected(14, Some(0)))),
        (pending(13, Some(0), 0), Some(injected(13, Some(1)))),
        (
            pending(13, Some(0), 0),
            Some(vp::PendingInterruption::Interrupt { vector: 0x30 }),
        ),
        (pending(13, Some(0), 0), Some(vp::PendingInterruption::Nmi)),
        // With a payload, which KVM keeps no more than it keeps a pending
        // exception.
        (pending(14, Some(2), 0x1000), Some(injected(14, Some(2)))),
    ] {
        // The refused restore changes no activity, the halt included, which a
        // restore that got as far as the MP state would have set.
        let crafted = vp::Activity {
            mp_state: vp::MpState::Halted,
            pending_event: Some(pending_event),
            pending_interruption,
            ..Default::default()
        };
        anyhow::ensure!(
            state.set_activity(&crafted).is_err(),
            "restoring {crafted:?} succeeded"
        );
        anyhow::ensure!(
            state.activity()? == before,
            "the refused restore of {crafted:?} changed the activity"
        );
    }
    Ok(())
}

// An earlier save recorded an exception that KVM had queued, but not injected
// yet, as pending and injected both. KVM restores that exception once, as
// injected, so it restores, and the next save records it as the pending
// interruption alone.
backend_test!(restores_a_queued_exception_as_injected, kvm_only, requires: []);
fn restores_a_queued_exception_as_injected(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    for error_code in [None, Some(0)] {
        let queued = vp::Activity {
            pending_event: Some(vp::PendingEvent::Exception {
                vector: 13,
                error_code,
                parameter: 0,
            }),
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector: 13,
                error_code,
            }),
            ..Default::default()
        };
        state
            .set_activity(&queued)
            .with_context(|| format!("restoring {queued:?} failed"))?;
        let injected = vp::Activity {
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector: 13,
                error_code,
            }),
            ..Default::default()
        };
        let restored = state.activity()?;
        anyhow::ensure!(
            restored == injected,
            "restoring {queued:?} read back as {restored:?}, not as {injected:?}"
        );
    }
    Ok(())
}
