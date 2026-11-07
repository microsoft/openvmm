// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the checks that adding a state unit makes before it changes the
//! set.

use super::SavedState;
use super::TestUnit;
use super::assert_no_dangling_edges;
use crate::SavedStateUnit;
use crate::StateTransitionErrorKind;
use crate::StateUnits;
use crate::UnitBuildError;
use crate::UnitHandle;
use crate::run_unit;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use test_with_tracing::test;
use vmcore::save_restore::SavedStateBlob;

/// The parts of a set that a failed build must leave unchanged.
#[derive(Debug, PartialEq)]
struct Inventory {
    next_id: u64,
    names: BTreeMap<String, u64>,
    edges: BTreeMap<u64, (BTreeSet<u64>, BTreeSet<u64>)>,
}

impl Inventory {
    fn of(units: &StateUnits) -> Self {
        let inner = units.inner.lock();
        Self {
            next_id: inner.next_id,
            names: inner
                .names
                .iter()
                .map(|(name, &id)| (name.to_string(), id))
                .collect(),
            edges: inner
                .units
                .iter()
                .map(|(&id, unit)| (id, (unit.dependencies.clone(), unit.dependents.clone())))
                .collect(),
        }
    }
}

fn add(units: &StateUnits, name: &str) -> UnitHandle {
    units.add(name).build(mesh::channel().0).unwrap()
}

fn id(handle: &UnitHandle) -> u64 {
    handle.id.id
}

/// Removing a unit that a builder refers to makes the build fail, and the
/// failed build leaves no name, edge, or ID behind for a later unit.
#[test]
fn a_removed_unit_fails_the_build_without_changing_the_set() {
    for removed_is_dependent in [false, true] {
        let units = StateUnits::new();
        let a = add(&units, "a");
        let b = add(&units, "b");
        let builder = units.add("x").depends_on(&a);
        let builder = if removed_is_dependent {
            builder.dependency_of(&b)
        } else {
            builder.depends_on(&b)
        };
        b.remove();
        let before = Inventory::of(&units);

        let err = builder.build(mesh::channel().0).unwrap_err();
        assert!(
            matches!(
                &err,
                UnitBuildError::RemovedUnit { unit, removed } if &**unit == "x" && &**removed == "b"
            ),
            "{err}"
        );
        assert_eq!(Inventory::of(&units), before);

        // The next unit takes the ID that x would have had, but none of its
        // edges, and the name x is free.
        let y = add(&units, "y");
        let x = units
            .add("x")
            .depends_on(&a)
            .build(mesh::channel().0)
            .unwrap();
        let inner = units.inner.lock();
        assert_eq!(inner.names["y"], id(&y));
        assert_eq!(inner.names["x"], id(&x));
        assert!(inner.units[&id(&y)].dependencies.is_empty());
        assert_eq!(inner.units[&id(&a)].dependents, BTreeSet::from([id(&x)]));
        assert_no_dangling_edges(&inner);
    }
}

/// Restore matches saved state to units by name, so a failed build must not
/// leave its name pointing at the ID that a later unit receives.
#[async_test]
async fn a_failed_build_does_not_route_saved_state_to_a_later_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let a = units
        .add("a")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let b = units
        .add("b")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let builder = units.add("x").depends_on(a.handle()).depends_on(b.handle());
    b.remove().await;
    builder.build(mesh::channel().0).unwrap_err();

    let restored = Arc::new(AtomicBool::new(false));
    let _y = units
        .add("y")
        .spawn(&driver, |recv| {
            run_unit(
                TestUnit {
                    value: restored.clone(),
                    dep: None,
                    support_saved_state: true,
                },
                recv,
            )
        })
        .unwrap();
    let err = units
        .restore(vec![SavedStateUnit {
            name: "x".to_owned(),
            state: SavedStateBlob::new(SavedState(true)),
        }])
        .await
        .unwrap_err();
    let StateTransitionErrorKind::Units(unit_errors) = err.error else {
        panic!("expected named unit failures");
    };
    assert_eq!(unit_errors.0.len(), 1);
    assert_eq!(unit_errors.0[0].0.as_ref(), "x");
    assert_eq!(unit_errors.0[0].1.to_string(), "unknown unit name");
    assert!(!restored.load(Ordering::Relaxed));
}

/// The set's lock orders a build and a concurrent removal of a unit that the
/// build refers to. If the build comes first, the removal prunes the new
/// unit's edge; otherwise the build fails without changing the set.
#[test]
fn a_removal_racing_a_build_leaves_the_set_consistent() {
    for _ in 0..25 {
        let units = StateUnits::new();
        let a = add(&units, "a");
        let b = add(&units, "b");
        let builder = units.add("x").depends_on(&a).dependency_of(&b);
        let barrier = &Barrier::new(2);
        let result = std::thread::scope(|scope| {
            scope.spawn(move || {
                barrier.wait();
                a.remove();
            });
            barrier.wait();
            builder.build(mesh::channel().0)
        });

        let inner = units.inner.lock();
        match &result {
            Ok(x) => {
                assert!(inner.units[&id(x)].dependencies.is_empty());
                assert_eq!(inner.units[&id(&b)].dependencies, BTreeSet::from([id(x)]));
            }
            Err(UnitBuildError::RemovedUnit { removed, .. }) if &**removed == "a" => {
                assert!(!inner.names.contains_key("x"));
                assert!(inner.units[&id(&b)].dependencies.is_empty());
                assert_eq!(inner.next_id, 2);
            }
            Err(err) => panic!("{err}"),
        }
        assert_eq!(inner.names.len(), inner.units.len());
        assert_no_dangling_edges(&inner);
    }
}

/// A build fails, without changing the set, if the new unit would depend,
/// directly or through other units, on a unit that it would also be a
/// dependency of, whether `depends_on` or `dependency_of` made the edges.
#[test]
fn dependency_cycles_fail_the_build_without_changing_the_set() {
    let units = StateUnits::new();
    // The dependency edges, each from a unit to one that it depends on, are
    // c -> b -> a -> e, where `e.dependency_of(a)` made the last one.
    let a = add(&units, "a");
    let b = units
        .add("b")
        .depends_on(&a)
        .build(mesh::channel().0)
        .unwrap();
    let c = units
        .add("c")
        .depends_on(&b)
        .build(mesh::channel().0)
        .unwrap();
    let e = units
        .add("e")
        .dependency_of(&a)
        .build(mesh::channel().0)
        .unwrap();
    let before = Inventory::of(&units);

    let cycles = [
        // x -> a -> x
        (units.add("x").depends_on(&a).dependency_of(&a), "a"),
        // x -> c -> b -> a -> x
        (units.add("x").depends_on(&c).dependency_of(&a), "a"),
        // x -> a -> e -> x, through the edge that `dependency_of` made
        (units.add("x").depends_on(&a).dependency_of(&e), "e"),
        // x -> c -> b -> x, with each edge requested twice
        (
            units
                .add("x")
                .depends_on(&c)
                .depends_on(&c)
                .dependency_of(&b)
                .dependency_of(&b),
            "b",
        ),
    ];
    for (builder, expected) in cycles {
        let err = builder.build(mesh::channel().0).unwrap_err();
        let UnitBuildError::DependencyCycle { unit, dependent } = &err else {
            panic!("{err}");
        };
        assert_eq!((&**unit, &**dependent), ("x", expected));
        assert_eq!(Inventory::of(&units), before);
    }

    // Paths beside existing ones form no cycle: b -> x -> a beside b -> a, and
    // c -> y -> b and c -> y -> e beside c -> b -> a -> e, where y reaches a
    // and e along two paths each.
    let x = units
        .add("x")
        .depends_on(&a)
        .dependency_of(&b)
        .build(mesh::channel().0)
        .unwrap();
    let y = units
        .add("y")
        .depends_on(&b)
        .depends_on(&e)
        .dependency_of(&c)
        .build(mesh::channel().0)
        .unwrap();
    let inner = units.inner.lock();
    assert_eq!(
        inner.units[&id(&b)].dependencies,
        BTreeSet::from([id(&a), id(&x)])
    );
    assert_eq!(
        inner.units[&id(&c)].dependencies,
        BTreeSet::from([id(&b), id(&y)])
    );
    assert_eq!(
        inner.units[&id(&y)].dependencies,
        BTreeSet::from([id(&b), id(&e)])
    );
    assert_no_dangling_edges(&inner);
}

/// Ordered state changes wait for a unit's dependencies, or for its dependents
/// when stopping, so the units of a cycle would wait for one another forever.
/// Adding a unit that would close a cycle fails, even while the units run, and
/// the set still stops and starts.
#[async_test]
async fn rejected_cycles_leave_ordered_state_changes_able_to_complete(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let a = units
        .add("a")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let b = units
        .add("b")
        .depends_on(a.handle())
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await;

    for builder in [
        units
            .add("x")
            .depends_on(a.handle())
            .dependency_of(a.handle()),
        units
            .add("x")
            .depends_on(b.handle())
            .dependency_of(a.handle()),
    ] {
        let err = builder
            .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
            .unwrap_err();
        assert!(
            matches!(err, UnitBuildError::DependencyCycle { .. }),
            "{err}"
        );
    }

    units.start_stopped_units().await;
    units.stop().await;
    units.reset().await.unwrap();
    units.start().await;
    units.stop().await;
}
