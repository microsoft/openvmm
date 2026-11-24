// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the activity of a WHP VP whose APIC the VMM
//! emulates, as `whp:user_mode_apic` has it.
//!
//! The VMM keeps such a VP's halt and its wait for a SIPI itself, and its APIC
//! holds the VP's pending NMI. The hypervisor holds the rest of the activity:
//! the interrupt state, and the pending interruption and event.

use super::fixture::Fixture;
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

/// The vectors of the SIPIs that start the AP, at `vector << 12`.
const SIPI_VECTORS: [u8; 2] = [0x08, 0x09];

/// An activity in each state that the VMM keeps, with every other part of the
/// activity set: an NMI pending in the APIC, and the interrupt state and a
/// pending exception, both as an interruption and as an event, in the
/// hypervisor. The pending interruption is not an interrupt, which WHP refuses
/// while the VP's interrupts are disabled, as a new VP's are.
fn activities() -> impl Iterator<Item = vp::Activity> {
    [
        vp::MpState::Running,
        vp::MpState::Halted,
        vp::MpState::WaitForSipi,
    ]
    .into_iter()
    .map(|mp_state| vp::Activity {
        mp_state,
        nmi_pending: true,
        nmi_masked: true,
        interrupt_shadow: true,
        pending_event: Some(vp::PendingEvent::Exception {
            vector: 14,
            error_code: Some(2),
            parameter: 0x1000,
        }),
        pending_interruption: Some(vp::PendingInterruption::Exception {
            vector: 13,
            error_code: Some(0),
        }),
        ..Default::default()
    })
}

// A restore sets the activity in part in the VMM and in part in the
// hypervisor, which denies a write of its internal activity state in this mode,
// even of the state that it holds. A restore of an activity must give the VP
// that activity, whatever its state, and so must a restore of a save of the VP
// into a new partition's VP, as a snapshot's restore does.
fn restores_the_activity_in_each_state<H: Hypervisor>(
    fixture: &Fixture,
    hv: &mut H,
) -> anyhow::Result<()>
where
    H::Partition: Partition + PartitionMemoryMapper,
{
    for activity in activities() {
        let saved = {
            let (_partition, mut binder) = fixture.build(hv, false)?;
            let mut processor = binder.bind()?;
            let mut state = processor.access_state(Vtl::Vtl0);
            state
                .set_activity(&activity)
                .with_context(|| format!("restoring {activity:?} failed"))?;
            let restored = state.activity()?;
            anyhow::ensure!(
                restored == activity,
                "restoring {activity:?} read back as {restored:?}"
            );
            mesh::payload::encode(state.save_all()?)
        };

        let (_partition, mut binder) = fixture.build(hv, false)?;
        let mut processor = binder.bind()?;
        let mut state = processor.access_state(Vtl::Vtl0);
        state
            .restore_all(&mesh::payload::decode::<vp::VpSavedState>(&saved)?)
            .with_context(|| {
                format!("restoring the save of {activity:?} into a new partition failed")
            })?;
        let restored = state.activity()?;
        anyhow::ensure!(
            restored == activity,
            "the save of {activity:?} restored into a new partition as {restored:?}"
        );
    }
    Ok(())
}

// A snapshot of a VM that has not run holds a new VP's state, which must
// restore into a new partition's VP, with Hv1 and without it.
fn restores_a_new_vp_into_a_new_partition<H: Hypervisor>(
    fixture: &Fixture,
    hv: &mut H,
    hv1: bool,
) -> anyhow::Result<()>
where
    H::Partition: Partition + PartitionMemoryMapper,
{
    let (saved, activity) = {
        let (_partition, mut binder) = fixture.build(hv, hv1)?;
        let mut processor = binder.bind()?;
        let mut state = processor.access_state(Vtl::Vtl0);
        (mesh::payload::encode(state.save_all()?), state.activity()?)
    };

    let (_partition, mut binder) = fixture.build(hv, hv1)?;
    let mut processor = binder.bind()?;
    let mut state = processor.access_state(Vtl::Vtl0);
    state
        .restore_all(&mesh::payload::decode::<vp::VpSavedState>(&saved)?)
        .context("restoring the save of a new VP into a new partition failed")?;
    let restored = state.activity()?;
    anyhow::ensure!(
        restored == activity,
        "the save of a new VP, {activity:?}, restored as {restored:?}"
    );
    Ok(())
}

/// Sends the AP, whose APIC ID is 1, an INIT or a SIPI, as the BSP does to
/// start it, and flushes the AP's requests, which has the AP take it, as it
/// would before it ran, and as it does before the VMM saves it. Returns the
/// AP's activity and registers then.
fn send(
    partition: &impl Partition,
    ap: &mut impl Processor,
    mode: DeliveryMode,
    vector: u8,
) -> anyhow::Result<(vp::Activity, vp::Registers)> {
    partition.request_msi(
        Vtl::Vtl0,
        MsiRequest::new_x86(mode, 1, false, vector, false),
    );
    ap.flush_async_requests();
    let mut state = ap.access_state(Vtl::Vtl0);
    Ok((state.activity()?, state.registers()?))
}

// A multiprocessor guest starts each AP with an INIT, which resets the AP and
// makes it wait for a SIPI, and then a SIPI, which starts it at the SIPI's
// vector. The VMM, which emulates the APIC, makes the AP wait for the SIPI
// itself. An INIT to a running AP resets it, and makes it wait again.
fn an_init_makes_an_ap_wait_for_a_sipi<H: Hypervisor>(
    fixture: &Fixture,
    hv: &mut H,
) -> anyhow::Result<()>
where
    H::Partition: Partition + PartitionMemoryMapper,
{
    let (partition, mut binders) = fixture.build_all(hv, false)?;
    let mut ap = binders.get_mut(1).context("the fixture has no AP")?.bind()?;
    let reset = ap.access_state(Vtl::Vtl0).registers()?;

    for vector in SIPI_VECTORS {
        let (activity, registers) = send(&partition, &mut ap, DeliveryMode::INIT, 0)?;
        anyhow::ensure!(
            (activity.mp_state, registers.rip) == (vp::MpState::WaitForSipi, reset.rip),
            "after an INIT, the AP is {:?} at {:#x}",
            activity.mp_state,
            registers.rip
        );

        let (activity, registers) = send(&partition, &mut ap, DeliveryMode::SIPI, vector)?;
        let (selector, base) = (registers.cs.selector, registers.cs.base);
        anyhow::ensure!(
            (activity.mp_state, selector, base, registers.rip)
                == (
                    vp::MpState::Running,
                    u16::from(vector) << 8,
                    u64::from(vector) << 12,
                    0
                ),
            "after a SIPI, the AP is {:?} at {selector:#x}:{:#x}, with CS's base {base:#x}",
            activity.mp_state,
            registers.rip
        );
    }
    Ok(())
}

fn supported() -> bool {
    // Evaluate the capabilities, which the test reads, as `backend_test!` does,
    // so that a failed probe fails the test.
    let _ = crate::native::whp_user_mode_apic::capabilities(false);
    true
}

fn supported_hv1() -> bool {
    let _ = crate::native::whp_user_mode_apic::capabilities(true);
    true
}

// These register by hand, since each builds two partitions, or one of two VPs.
petri::multitest!(vec![
    crate::native::whp_user_mode_apic::test(
        "whp::restores_the_activity_in_each_state",
        false,
        supported,
        async |driver, mut hv| {
            restores_the_activity_in_each_state(&Fixture::new(driver).await?, &mut hv)
        },
    ),
    crate::native::whp_user_mode_apic::test(
        "whp::restores_a_new_vp_into_a_new_partition",
        false,
        supported,
        async |driver, mut hv| {
            restores_a_new_vp_into_a_new_partition(&Fixture::new(driver).await?, &mut hv, false)
        },
    ),
    crate::native::whp_user_mode_apic::test(
        "whp::restores_a_new_vp_into_a_new_partition_hv1",
        true,
        supported_hv1,
        async |driver, mut hv| {
            restores_a_new_vp_into_a_new_partition(&Fixture::new(driver).await?, &mut hv, true)
        },
    ),
    crate::native::whp_user_mode_apic::test(
        "whp::an_init_makes_an_ap_wait_for_a_sipi",
        false,
        supported,
        async |driver, mut hv| {
            an_init_makes_an_ap_wait_for_a_sipi(&Fixture::with_vps(driver, 2).await?, &mut hv)
        },
    ),
]);
