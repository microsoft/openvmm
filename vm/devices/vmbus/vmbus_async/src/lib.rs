// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus channel implementations using async.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod async_dgram;
mod core;
pub mod pipe;
pub mod queue;
