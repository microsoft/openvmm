// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial port backends based on Unix sockets and Windows named pipes.

pub mod unix;
#[cfg(windows)]
pub mod windows;
