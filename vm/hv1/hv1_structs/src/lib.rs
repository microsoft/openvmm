// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data structures that may be useful when working with hv1_hypercall.

#![forbid(unsafe_code)]

mod proc_mask;
mod vtl_array;

pub use proc_mask::*;
pub use vtl_array::*;
