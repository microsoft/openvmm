// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust binadings to the `vmservice.proto` TTRPC API

#![forbid(unsafe_code)]
#![allow(clippy::enum_variant_names, clippy::large_enum_variant, non_snake_case)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use mesh_rpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/vmservice.rs"));
