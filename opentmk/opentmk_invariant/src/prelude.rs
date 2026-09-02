// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is an extended prelude crate that imports a number of common rust API entities that
//! would otherwise be imported from the `alloc` crate.
#[cfg(target_os = "uefi")]
extern crate alloc;
#[cfg(target_os = "uefi")]
pub use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};

#[cfg(not(target_os = "uefi"))]
pub use std::sync::Arc;
