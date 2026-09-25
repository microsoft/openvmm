// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the state-unit inventory.

use super::TestUnit;
use crate::StateUnits;
use crate::run_unit;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;

#[async_test]
async fn test_exact_inventory_validation(driver: DefaultDriver) {
    let units = StateUnits::new();
    let _first = units
        .add("first")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let _second = units
        .add("second")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();

    assert_eq!(units.inventory(), ["first", "second"]);
    units
        .validate_inventory(&["first".to_owned(), "second".to_owned()])
        .unwrap();
    assert!(units.validate_inventory(&["first".to_owned()]).is_err());
    assert!(
        units
            .validate_inventory(&["second".to_owned(), "first".to_owned()])
            .is_err()
    );
    assert!(
        units
            .validate_inventory(&["first".to_owned(), "second".to_owned(), "extra".to_owned(),])
            .is_err()
    );
}
