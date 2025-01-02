// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core mesh channel functionality.

#![warn(missing_docs)]

mod deque;
mod error;
mod mpsc;

pub use error::*;
pub use mpsc::*;
