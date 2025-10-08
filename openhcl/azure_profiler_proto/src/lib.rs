// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Azure Profiler service protocol definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use inspect as _;
use mesh_rpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/profile.rs"));
