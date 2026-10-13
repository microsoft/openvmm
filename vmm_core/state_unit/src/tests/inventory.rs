// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the state-unit inventory.

use super::TestUnit;
use crate::StateRequest;
use crate::StateUnits;
use crate::run_unit;
use futures::StreamExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use test_with_tracing::test;

#[async_test]
async fn test_exact_inventory_validation(driver: DefaultDriver) {
    let units = StateUnits::new();
    let _second = units
        .add("second")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let _first = units
        .add("first")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();

    assert_eq!(units.inventory(), ["first", "second"]);
    units
        .validate_inventory(&["first".to_owned(), "second".to_owned()])
        .unwrap();
    units
        .validate_inventory(&["second".to_owned(), "first".to_owned()])
        .unwrap();
    assert!(units.validate_inventory(&["first".to_owned()]).is_err());
    assert!(
        units
            .validate_inventory(&["first".to_owned(), "first".to_owned()])
            .is_err()
    );
    assert!(
        units
            .validate_inventory(&["first".to_owned(), "second".to_owned(), "extra".to_owned(),])
            .is_err()
    );
}

#[async_test]
async fn test_save_inventory_uses_unit_snapshot(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let (send, mut recv) = mesh::channel();
    let handle = units.add("removed").build(send).unwrap();
    let (save_started_send, save_started_recv) = mesh::oneshot();
    let (resume_save_send, resume_save_recv) = mesh::oneshot();
    let unit_task = driver.spawn("blocking state unit", async move {
        let request = recv.next().await.unwrap();
        assert!(matches!(&request, StateRequest::Save(_)));
        save_started_send.send(());
        resume_save_recv.await.unwrap();
        request
            .apply(&mut TestUnit {
                support_saved_state: true,
                ..Default::default()
            })
            .await;
    });

    let save_task = driver.spawn("save state units", async move {
        units.save_with_inventory().await.unwrap()
    });
    save_started_recv.await.unwrap();
    handle.remove();
    resume_save_send.send(());

    let (states, inventory) = save_task.await;
    unit_task.await;
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].name, "removed");
    assert_eq!(inventory, ["removed"]);
}
