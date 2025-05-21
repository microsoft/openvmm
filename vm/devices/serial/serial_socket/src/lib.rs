// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial port backends based on sockets and Windows named pipes.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod net;
#[cfg(windows)]
pub mod windows;
