// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Simple tests common to all architectures.

use crate::tests::prelude::*;

#[tmk_test]
fn boot() {
    log!("hello world");
}
