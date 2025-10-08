// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust binadings to the `vmservice.proto` TTRPC API

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![expect(clippy::enum_variant_names, clippy::large_enum_variant)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use mesh_rpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/vmservice.rs"));
