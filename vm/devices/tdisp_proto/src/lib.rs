// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDISP guest-to-host command protocol definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use inspect as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/tdisp.rs"));

/// Major version of the TDISP guest-to-host interface.
pub const TDISP_INTERFACE_VERSION_MAJOR: u32 = 1;

/// Minor version of the TDISP guest-to-host interface.
pub const TDISP_INTERFACE_VERSION_MINOR: u32 = 0;
