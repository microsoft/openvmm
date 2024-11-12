// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial port backends based on sockets and Windows named pipes.

pub mod net;
#[cfg(windows)]
pub mod windows;
