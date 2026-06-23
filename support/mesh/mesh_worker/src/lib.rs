// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for workers, which are agents that mostly communicate via
//! mesh message passing. These provide a way for splitting up your program into
//! separable components, each of which can optionally run in a separate
//! process.
//!
//! # Overview
//!
//! A worker is a self-contained component that:
//! 1. Is created from `Parameters` ([`Worker::new`])
//! 2. Runs a main loop consuming `WorkerRpc` messages ([`Worker::run`])
//! 3. Can be saved and restarted from `State` ([`Worker::restart`])
//!
//! Workers are registered at compile time with [`register_workers!`] and
//! launched at runtime through a [`WorkerHost`]. The host can run workers
//! in-process (on a new thread) or in a child process via `mesh_process`.
//!
//! # Entry points
//!
//! - [`Worker`] trait — define a worker's parameters, state, and behavior.
//! - [`worker_host()`] — create a `(WorkerHost, WorkerHostRunner)` pair.
//! - [`register_workers!`] — register worker types for dynamic lookup by name.
//! - [`RegisteredWorkers`] — factory that resolves worker names to builders,
//!   using the compile-time registry.
//! - [`WorkerHandle`] — handle to a running worker, supporting stop, restart,
//!   inspect, and lifetime events.

mod worker;

// TODO: flatten this module.
pub use worker::*;
