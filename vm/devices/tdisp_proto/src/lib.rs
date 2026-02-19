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
