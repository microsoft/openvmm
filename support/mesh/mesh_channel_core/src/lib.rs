// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core mesh channel functionality, supporting sending and receiving messages
//! within and between nodes.
//!
//! This contains only the basic channel implementations, not the extra utility
//! types on top that `mesh_channel` provides.

#![warn(missing_docs)]

mod deque;
mod error;
mod mpsc;
mod oneshot;
mod sync_unsafe_cell;

pub use error::*;
pub use mpsc::*;
pub use oneshot::*;
