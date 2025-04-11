// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Imports that are commonly needed in tests.
//!
//! Test modules should typically import this module:
//! ```rust
//! use crate::prelude::*;
//! ```

#![allow(unused_imports)]

pub use tmk_core::Scope;
pub use tmk_core::TestContext;
pub use tmk_core::log;
#[cfg(target_arch = "x86_64")]
pub use tmk_core::x86_64::IsrContext;
pub use tmk_macros::tmk_test;
