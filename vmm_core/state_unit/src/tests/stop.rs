// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for a stop that fails instead of panicking.

use super::start::recording_unit;
use super::start::unit_state;
use crate::State;
use crate::StateRequest;
use crate::StateUnits;
use crate::run_unit;
use futures::StreamExt;
use mesh::Receiver;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Runs a unit whose task answers its start and the confirmation of that
/// start, and then exits, as a task that has failed would.
async fn run_lost_after_start(mut recv: Receiver<StateRequest>) {
    while let Some(request) = recv.next().await {
        match request {
            StateRequest::Start(response) => response.complete(()),
            StateRequest::ConfirmStart(response) => {
                response.complete(Ok(()));
                break;
            }
            request => panic!("unexpected request {request:?}"),
        }
    }
}

#[async_test]
async fn a_lost_unit_fails_the_stop_and_keeps_its_dependencies_running(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_stopped = flag();
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &dependency_stopped), recv)
        })
        .unwrap();
    let lost = units
        .add("lost")
        .depends_on(dependency.handle())
        .spawn(&driver, run_lost_after_start)
        .unwrap();
    let dependent_stopped = flag();
    let _dependent = units
        .add("dependent")
        .depends_on(lost.handle())
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &dependent_stopped), recv)
        })
        .unwrap();
    let independent_stopped = flag();
    let _independent = units
        .add("independent")
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &independent_stopped), recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units.stop().await.unwrap_err().to_string();
    assert!(error.contains("lost"), "{error}");
    assert!(
        error.contains("dependency: stop skipped because a dependent was not stopped"),
        "{error}"
    );
    assert!(dependent_stopped.load(Ordering::Relaxed));
    assert!(independent_stopped.load(Ordering::Relaxed));
    // The lost unit may still use its dependency, so the dependency runs.
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "lost"), State::StopUncertain);
    assert_eq!(unit_state(&units, "dependency"), State::Running);
    assert_eq!(unit_state(&units, "dependent"), State::Stopped);
    assert_eq!(unit_state(&units, "independent"), State::Stopped);

    // The units can never start again, and the requests that need stopped
    // units fail, rather than panic, while the units stay running.
    let start_error = units.start().await.unwrap_err();
    assert!(start_error.to_string().contains("terminal state"));
    assert_eq!(unit_state(&units, "independent"), State::Stopped);
    let reset_error = format!(
        "{:#}",
        anyhow::Error::from(units.reset().await.unwrap_err())
    );
    assert!(
        reset_error.contains("lost: unit is in terminal state"),
        "{reset_error}"
    );
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
    assert!(units.advance_time(Duration::from_secs(1)).await.is_err());

    // A later stop fails the same way while the lost unit remains.
    assert!(units.stop().await.is_err());
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());

    // Once the lost unit is removed, the stop can stop its dependency, and
    // the remaining units, none of which is in a terminal state, can start.
    lost.remove().await;
    units.stop().await.unwrap();
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    units.start().await.unwrap();
    units.stop().await.unwrap();
}

#[async_test]
async fn a_unit_whose_stop_is_unanswered_can_be_stopped_again(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_stopped = flag();
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &dependency_stopped), recv)
        })
        .unwrap();
    let stopped = flag();
    let stop_requests = Arc::new(AtomicUsize::new(0));
    // A unit that drops its first stop request without stopping or answering.
    let _unanswered = units
        .add("unanswered")
        .depends_on(dependency.handle())
        .spawn(&driver, |mut recv| {
            let mut unit = recording_unit(&flag(), &stopped);
            let stop_requests = stop_requests.clone();
            async move {
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Stop(response)
                            if stop_requests.fetch_add(1, Ordering::Relaxed) == 0 =>
                        {
                            drop(response);
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
                unit
            }
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units.stop().await.unwrap_err().to_string();
    assert!(error.contains("unanswered"), "{error}");
    assert!(!stopped.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "unanswered"), State::StopUncertain);
    assert_eq!(unit_state(&units, "dependency"), State::Running);

    // The unit may be running, so a later stop sends it another request, and
    // the unit keeps its terminal marker once it stops.
    units.stop().await.unwrap();
    assert_eq!(stop_requests.load(Ordering::Relaxed), 2);
    assert!(stopped.load(Ordering::Relaxed));
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "unanswered"), State::StopUncertain);
    assert_eq!(unit_state(&units, "dependency"), State::Stopped);
    let start_error = units.start().await.unwrap_err();
    assert!(start_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
    assert!(units.advance_time(Duration::from_secs(1)).await.is_err());
}

#[async_test]
async fn a_unit_removed_during_a_stop_does_not_block_its_dependencies(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_stopped = flag();
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &dependency_stopped), recv)
        })
        .unwrap();
    let (stop_received_send, stop_received) = mesh::oneshot();
    // A unit that holds its stop request without answering it until the unit
    // is removed, which ends its task.
    let removed = units
        .add("removed")
        .depends_on(dependency.handle())
        .spawn(&driver, |mut recv| {
            let mut unit = recording_unit(&flag(), &flag());
            async move {
                let mut stop_received_send = Some(stop_received_send);
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Stop(_response) => {
                            stop_received_send.take().unwrap().send(());
                            std::future::pending::<()>().await;
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
            }
        })
        .unwrap();
    units.start().await.unwrap();

    let remove = async {
        stop_received.await.unwrap();
        drop(removed);
    };
    let (result, ()) = futures::join!(units.stop(), remove);
    result.unwrap();
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "dependency"), State::Stopped);
}

#[async_test]
async fn a_unit_removed_while_blocked_does_not_block_its_dependencies(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_stopped = flag();
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &dependency_stopped), recv)
        })
        .unwrap();
    let removed = units
        .add("removed")
        .depends_on(dependency.handle())
        .spawn(&driver, |recv| {
            run_unit(recording_unit(&flag(), &flag()), recv)
        })
        .unwrap();
    let (stop_received_send, stop_received) = mesh::oneshot();
    let (release_send, release) = mesh::oneshot::<()>();
    // A unit that depends on `removed` and holds its stop request until
    // `removed` is removed, and then exits without answering it, so that
    // `removed` waits for it and is blocked.
    let _unanswered = units
        .add("unanswered")
        .depends_on(removed.handle())
        .spawn(&driver, |mut recv| {
            let mut unit = recording_unit(&flag(), &flag());
            async move {
                let mut stop_received_send = Some(stop_received_send);
                let mut release = Some(release);
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Stop(response) => {
                            stop_received_send.take().unwrap().send(());
                            release.take().unwrap().await.unwrap();
                            drop(response);
                            break;
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
            }
        })
        .unwrap();
    units.start().await.unwrap();

    let remove = async {
        stop_received.await.unwrap();
        drop(removed);
        release_send.send(());
    };
    let (result, ()) = futures::join!(units.stop(), remove);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("unanswered"), "{error}");
    assert!(!error.contains("dependency"), "{error}");
    // The removed unit no longer uses its dependency, so it stops.
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert_eq!(unit_state(&units, "dependency"), State::Stopped);
    assert_eq!(unit_state(&units, "unanswered"), State::StopUncertain);
}
