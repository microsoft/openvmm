// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Simple tests common to all architectures.

use crate::prelude::*;

#[tmk_test]
fn boot(_: TestContext<'_>) {
    log!("hello world");
}
