// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for a reset, save, restore, or time advance that a unit whose task
//! has exited does not answer.

use super::TestUnit;
use super::start::unit_state;
use crate::SavedStateUnit;
use crate::State;
use crate::StateRequest;
use crate::StateTransitionError;
use crate::StateTransitionErrorKind;
use crate::StateUnits;
use crate::UnitErrorSet;
use crate::UnitHandle;
use crate::run_unit;
use futures::StreamExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SavedStateBlob;

/// Adds a unit named `name` whose task has exited: its request receiver is
/// dropped.
fn lost_unit(units: &StateUnits, name: &str) -> UnitHandle {
    let (send, recv) = mesh::channel();
    drop(recv);
    units.add(name).build(send).unwrap()
}

/// Checks that `error` is the failure of `op` because the unit named `name`,
/// alone, did not answer its request.
fn assert_unanswered(error: StateTransitionError, op: &str, name: &str) {
    let StateTransitionErrorKind::Units(UnitErrorSet(errors)) = &error.error else {
        panic!("{op} failed for another reason: {error:?}");
    };
    let names = errors
        .iter()
        .map(|(name, _)| name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        (error.op, names.as_slice()),
        (op, [name].as_slice()),
        "{error:?}"
    );
    let message = format!("{:#}", errors[0].1);
    assert!(
        message.contains("critical unit communication failure"),
        "{message}"
    );
}

/// Checks that a start, reset, save, restore, and time advance of `units`
/// each fail because the unit named `name` is in a terminal state.
async fn assert_terminal(units: &mut StateUnits, name: &str) {
    let error = format!("{:#}", units.start().await.unwrap_err());
    assert!(
        error.contains(&format!("'{name}' cannot be started from terminal state")),
        "{error}"
    );
    for error in [
        units.reset().await.unwrap_err(),
        units.save().await.err().unwrap(),
        units.restore(Vec::new()).await.unwrap_err(),
        units
            .advance_time(Duration::from_secs(1))
            .await
            .unwrap_err(),
    ] {
        let error = format!("{:#}", anyhow::Error::from(error));
        assert!(
            error.contains(&format!("{name}: unit is in terminal state")),
            "{error}"
        );
    }
}

/// The saved state of the unit named `name`.
fn saved_state(name: &str) -> SavedStateUnit {
    SavedStateUnit {
        name: name.to_owned(),
        state: SavedStateBlob::new(NoSavedState),
    }
}

#[async_test]
async fn a_lost_unit_fails_a_save(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _healthy = units
        .add("healthy")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let _lost = lost_unit(&units, "lost");

    let Err(error) = units.save().await else {
        panic!("save succeeded");
    };
    assert_unanswered(error, "save", "lost");
    assert_eq!(unit_state(&units, "lost"), State::SaveUncertain);
    assert_eq!(unit_state(&units, "healthy"), State::Stopped);
    assert_terminal(&mut units, "lost").await;
}

#[async_test]
async fn a_lost_unit_fails_a_reset(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let lost = lost_unit(&units, "lost");
    // A unit that depends on the lost unit still resets after it.
    let _dependent = units
        .add("dependent")
        .depends_on(&lost)
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();

    assert_unanswered(units.reset().await.unwrap_err(), "reset", "lost");
    assert_eq!(unit_state(&units, "lost"), State::ResetUncertain);
    assert_eq!(unit_state(&units, "dependent"), State::Stopped);
    assert_terminal(&mut units, "lost").await;
}

#[async_test]
async fn a_lost_unit_fails_a_restore(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _healthy = units
        .add("healthy")
        .spawn(&driver, |recv| {
            run_unit(
                TestUnit {
                    support_saved_state: true,
                    ..Default::default()
                },
                recv,
            )
        })
        .unwrap();
    let _lost = lost_unit(&units, "lost");
    let healthy_state = SavedStateUnit {
        name: "healthy".to_owned(),
        state: SavedStateBlob::new(super::SavedState(true)),
    };

    assert_unanswered(
        units
            .restore(vec![healthy_state, saved_state("lost")])
            .await
            .unwrap_err(),
        "restore",
        "lost",
    );
    assert_eq!(unit_state(&units, "lost"), State::RestoreUncertain);
    assert_eq!(unit_state(&units, "healthy"), State::Stopped);
    assert_terminal(&mut units, "lost").await;
}

#[async_test]
async fn a_lost_unit_fails_a_time_advance(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _healthy = units
        .add("healthy")
        .advances_time()
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let (send, recv) = mesh::channel();
    drop(recv);
    let _lost = units.add("lost").advances_time().build(send).unwrap();

    assert_unanswered(
        units
            .advance_time(Duration::from_secs(1))
            .await
            .unwrap_err(),
        "advance_time",
        "lost",
    );
    assert_eq!(unit_state(&units, "lost"), State::AdvanceTimeUncertain);
    assert_eq!(unit_state(&units, "healthy"), State::Stopped);
    assert_terminal(&mut units, "lost").await;
}

#[async_test]
async fn a_unit_removed_during_a_save_does_not_fail_it(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _healthy = units
        .add("healthy")
        .spawn(&driver, |recv| {
            run_unit(
                TestUnit {
                    support_saved_state: true,
                    ..Default::default()
                },
                recv,
            )
        })
        .unwrap();
    let (save_received_send, save_received) = mesh::oneshot();
    // A unit that holds its save request without answering it until the unit
    // is removed, which ends its task.
    let removed = units
        .add("removed")
        .spawn(&driver, |mut recv| async move {
            let mut save_received_send = Some(save_received_send);
            while let Some(request) = recv.next().await {
                if let StateRequest::Save(_response) = request {
                    save_received_send.take().unwrap().send(());
                    std::future::pending::<()>().await;
                }
            }
        })
        .unwrap();

    let remove = async {
        save_received.await.unwrap();
        drop(removed);
    };
    let (result, ()) = futures::join!(units.save(), remove);
    let Ok(states) = result else {
        panic!("save failed");
    };
    let names = states
        .iter()
        .map(|state| state.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["healthy"]);
    assert_eq!(unit_state(&units, "healthy"), State::Stopped);
    units.start().await.unwrap();
}
