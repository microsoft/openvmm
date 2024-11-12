// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code which resolves user-defined
//! [`Pipeline`](flowey_core::pipeline::Pipeline) objects into runnable code.
//!
//! Depending on the selected backend, different [`crate::flow_resolver`]
//! implementations will be used.

pub mod ado_yaml;
// pub mod bash; // not maintained at the moment
pub mod common_yaml;
pub mod direct_run;
pub mod generic;
pub mod github_yaml;
pub mod viz;
