// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the state-unit inventory: the contracts that units declare, that
//! a save records with their saved state, and that a restore checks.

use super::TestUnit;
use crate::InventoryMismatch;
use crate::StateRequest;
use crate::StateUnits;
use crate::UnitContract;
use crate::UnitDescriptor;
use crate::UnitLifetime;
use crate::run_unit;
use futures::StreamExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use test_with_tracing::test;

fn contract(
    name: &str,
    stable_id: &str,
    kind: &str,
    lifetime: UnitLifetime,
    descriptors: &[(&str, &str)],
) -> UnitContract {
    UnitContract {
        name: name.to_owned(),
        stable_id: stable_id.to_owned(),
        kind: kind.to_owned(),
        lifetime,
        descriptors: descriptors
            .iter()
            .map(|&(key, value)| UnitDescriptor::new(key, value))
            .collect(),
    }
}

/// A plain static unit named `name`, as a default declaration records it.
fn plain(name: &str) -> UnitContract {
    contract(name, name, "", UnitLifetime::Static, &[])
}

/// Adds a unit with no saved state, declared as `contract` declares it.
fn add(units: &StateUnits, driver: &DefaultDriver, contract: &UnitContract) -> crate::UnitHandle {
    let (send, recv) = mesh::channel();
    let handle = units
        .add(contract.name.clone())
        .stable_id(contract.stable_id.clone())
        .kind(contract.kind.clone())
        .lifetime(contract.lifetime)
        .descriptors(contract.descriptors.iter().cloned())
        .build(send)
        .unwrap();
    driver
        .spawn("unit", run_unit(TestUnit::default(), recv))
        .detach();
    handle
}

#[async_test]
async fn inventory_records_declared_contracts_in_name_order(driver: DefaultDriver) {
    let units = StateUnits::new();
    let _second = units
        .add("second")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    // Descriptors are recorded in canonical order, without duplicates.
    let _first = units
        .add("first")
        .stable_id("pcie-port:rp0")
        .kind("virtio")
        .lifetime(UnitLifetime::Dynamic)
        .descriptor("pci.id", "1af4:1044")
        .descriptor("irq", "5")
        .descriptor("pci.id", "1af4:1044")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();

    assert_eq!(
        units.inventory(),
        [
            contract(
                "first",
                "pcie-port:rp0",
                "virtio",
                UnitLifetime::Dynamic,
                &[("irq", "5"), ("pci.id", "1af4:1044")],
            ),
            plain("second"),
        ]
    );
}

#[async_test]
async fn check_ignores_registration_order(driver: DefaultDriver) {
    let pit = contract("pit", "pit", "", UnitLifetime::Static, &[("pio", "0x40+4")]);
    let lines = contract(
        "lines/ioapic",
        "lines/ioapic",
        "",
        UnitLifetime::Derived,
        &[],
    );

    let source = StateUnits::new();
    let _source_units = [add(&source, &driver, &pit), add(&source, &driver, &lines)];
    let destination = StateUnits::new();
    let _destination_units = [
        add(&destination, &driver, &lines),
        add(&destination, &driver, &pit),
    ];

    let plan = destination
        .check_inventory(&source.inventory(), &[])
        .unwrap();
    assert!(plan.recreate.is_empty());

    // The inventory's own order does not matter either.
    let mut reversed = source.inventory();
    reversed.reverse();
    destination.check_inventory(&reversed, &[]).unwrap();
}

#[async_test]
async fn check_reports_every_mismatch(driver: DefaultDriver) {
    let derived = contract("vmtime", "vmtime", "", UnitLifetime::Derived, &[]);
    let saved_pit = contract("pit", "pit", "", UnitLifetime::Static, &[("pio", "0x40+4")]);
    let current_pit = contract("pit", "pit", "", UnitLifetime::Static, &[("pio", "0x48+4")]);

    let destination = StateUnits::new();
    let _units = [
        add(&destination, &driver, &derived),
        add(&destination, &driver, &current_pit),
        add(&destination, &driver, &plain("rtc")),
        add(&destination, &driver, &plain("extra")),
    ];

    let inventory = [
        derived.clone(),
        saved_pit.clone(),
        plain("rtc"),
        plain("rtc"),
        plain("missing"),
    ];
    let states = [crate::SavedStateUnit {
        name: "ghost".to_owned(),
        state: vmcore::save_restore::SavedStateBlob::new(vmcore::save_restore::NoSavedState),
    }];
    let error: InventoryMismatch = destination
        .check_inventory(&inventory, &states)
        .unwrap_err();

    assert_eq!(error.missing, [plain("missing")]);
    assert_eq!(error.unexpected, [plain("extra")]);
    assert_eq!(error.changed, [(saved_pit, current_pit)]);
    assert_eq!(error.duplicates, ["rtc"]);
    assert_eq!(error.unrecorded, ["ghost"]);
    assert_eq!(
        error.to_string(),
        "the saved state-unit inventory does not match the VM: \
         missing [missing (static)]; \
         unexpected [extra (static)]; \
         changed [saved pit (static, pio=0x40+4) is now pit (static, pio=0x48+4)]; \
         recorded more than once [rtc]; \
         saved state without a record [ghost]"
    );
}

#[async_test]
async fn check_plans_recreating_missing_dynamic_units(driver: DefaultDriver) {
    let fixed = plain("fixed");
    let hot = contract(
        "pcie-hotplug:rp0",
        "pcie-port:rp0",
        "virtio",
        UnitLifetime::Dynamic,
        &[("pci.id", "1af4:1044")],
    );

    let destination = StateUnits::new();
    let _fixed = add(&destination, &driver, &fixed);
    let inventory = [fixed.clone(), hot.clone()];

    // A destination without the hot-plugged unit re-creates it.
    let plan = destination.check_inventory(&inventory, &[]).unwrap();
    assert_eq!(plan.recreate, std::slice::from_ref(&hot));

    // Once re-created as recorded, the inventory matches.
    let recreated = add(&destination, &driver, &hot);
    let plan = destination.check_inventory(&inventory, &[]).unwrap();
    assert!(plan.recreate.is_empty());
    recreated.remove();

    // A re-created unit that differs from its record does not.
    let mut other_device = hot.clone();
    other_device.descriptors = vec![UnitDescriptor::new("pci.id", "1b36:0010")];
    let _other_device = add(&destination, &driver, &other_device);
    let error = destination.check_inventory(&inventory, &[]).unwrap_err();
    assert_eq!(error.changed, [(hot, other_device)]);

    // A dynamic unit that the snapshot does not record is unexpected too.
    let error = destination.check_inventory(&[fixed], &[]).unwrap_err();
    assert_eq!(error.unexpected.len(), 1);
    assert_eq!(error.unexpected[0].lifetime, UnitLifetime::Dynamic);
}

#[async_test]
async fn save_records_every_unit_with_its_state(driver: DefaultDriver) {
    let mut source = StateUnits::new();
    let value = Arc::new(AtomicBool::new(true));
    let _stateful = source
        .add("stateful")
        .lifetime(UnitLifetime::Dynamic)
        .spawn(&driver, |recv| {
            run_unit(
                TestUnit {
                    value: value.clone(),
                    support_saved_state: true,
                    ..Default::default()
                },
                recv,
            )
        })
        .unwrap();
    let _stateless = source
        .add("stateless")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();

    let (states, inventory) = source.save_with_inventory().await.unwrap();
    assert_eq!(
        states.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["stateful"]
    );
    assert_eq!(
        inventory,
        [
            contract("stateful", "stateful", "", UnitLifetime::Dynamic, &[]),
            plain("stateless"),
        ]
    );

    // A destination re-creates the dynamic unit from its record, then
    // restores both.
    let mut destination = StateUnits::new();
    let _stateless = destination
        .add("stateless")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let plan = destination.check_inventory(&inventory, &states).unwrap();
    assert_eq!(
        plan.recreate
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["stateful"]
    );
    let restored = Arc::new(AtomicBool::new(false));
    let recreated = &plan.recreate[0];
    let _stateful = destination
        .add(recreated.name.clone())
        .lifetime(recreated.lifetime)
        .spawn(&driver, |recv| {
            run_unit(
                TestUnit {
                    value: restored.clone(),
                    support_saved_state: true,
                    ..Default::default()
                },
                recv,
            )
        })
        .unwrap();
    assert!(
        destination
            .check_inventory(&inventory, &states)
            .unwrap()
            .recreate
            .is_empty()
    );
    destination.restore(states).await.unwrap();
    assert!(restored.load(Ordering::Relaxed));
}

/// Saves with a unit that `remove` removes while the save waits for it, and
/// that then answers the save or, if `answer` is false, drops it.
async fn save_while_removing(
    driver: &DefaultDriver,
    answer: bool,
) -> (Vec<crate::SavedStateUnit>, Vec<UnitContract>) {
    let mut units = StateUnits::new();
    let _kept = units
        .add("kept")
        .spawn(driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let (send, mut recv) = mesh::channel();
    let handle = units.add("removed").build(send).unwrap();
    let (save_started_send, save_started_recv) = mesh::oneshot();
    let (resume_save_send, resume_save_recv) = mesh::oneshot::<()>();
    let unit_task = driver.spawn("blocking state unit", async move {
        let request = recv.next().await.unwrap();
        assert!(matches!(&request, StateRequest::Save(_)));
        save_started_send.send(());
        resume_save_recv.await.unwrap();
        if answer {
            request
                .apply(&mut TestUnit {
                    support_saved_state: true,
                    ..Default::default()
                })
                .await;
        } else {
            drop(request);
        }
    });

    let save_task = driver.spawn("save state units", async move {
        units.save_with_inventory().await.unwrap()
    });
    save_started_recv.await.unwrap();
    handle.remove();
    resume_save_send.send(());
    unit_task.await;
    save_task.await
}

#[async_test]
async fn save_records_a_unit_removed_after_it_answered(driver: DefaultDriver) {
    let (states, inventory) = save_while_removing(&driver, true).await;
    assert_eq!(
        states.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["removed"]
    );
    assert_eq!(inventory, [plain("kept"), plain("removed")]);
}

#[async_test]
async fn save_omits_a_unit_removed_before_it_answered(driver: DefaultDriver) {
    let (states, inventory) = save_while_removing(&driver, false).await;
    assert!(states.is_empty());
    assert_eq!(inventory, [plain("kept")]);

    // A destination that still has the removed unit does not match the
    // snapshot, rather than restoring it in its initial state.
    let destination = StateUnits::new();
    let _units = [
        add(&destination, &driver, &plain("kept")),
        add(&destination, &driver, &plain("removed")),
    ];
    let error = destination
        .check_inventory(&inventory, &states)
        .unwrap_err();
    assert_eq!(error.unexpected, [plain("removed")]);
}
