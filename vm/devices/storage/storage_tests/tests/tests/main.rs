// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for storage devices that don't qualify as unit tests, including
//! integration tests.
//!
//! Everything here builds into a single test binary. The modules live in this
//! subdirectory rather than directly under `tests/` so that Cargo does not also
//! auto-discover each of them as a test target of its own, which would compile
//! them, and the shared helpers they use, once per binary.

mod disk_sector_range;
mod http_server;
mod storvsc;

// The emulated NVMe harness and its only consumers are limited to the platforms
// the user-mode driver supports.
#[cfg(any(windows, target_os = "linux"))]
mod common;
#[cfg(any(windows, target_os = "linux"))]
mod scsidvd_nvme;
