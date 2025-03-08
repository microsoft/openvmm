// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Underhill diagnostics server protocol definitions.

#![expect(missing_docs)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use inspect as _;
use mesh_rpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/diag.rs"));

/// The AF_VSOCK port number the server runs on.
///
/// Happens to be the address of Bag End.
pub const VSOCK_CONTROL_PORT: u32 = 1;
pub const VSOCK_DATA_PORT: u32 = 2;

/// The maximum length of a file line.
pub const FILE_LINE_MAX: usize = 2048;
