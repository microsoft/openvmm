// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for workers, which are agents that mostly communicate via
//! mesh message passing. These provide a way for splitting up your program into
//! separable components, each of which can optionally run in a separate
//! process.
//!
//! The primary entry points are the [`worker_host()`] function, used to launch
//! workers, and the [`Worker`] trait, used to define workers. The
//! [`register_workers`] macro and the [`RegisteredWorkers`] factory are helpful
//! for defining the possible workers in a binary.

#![warn(missing_docs)]

mod worker;

// TODO: flatten this module.
pub use worker::*;
