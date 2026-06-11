// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator: launches a controlled environment in which to run petri test
//! commands.
//!
//! An incubator is the place a test "culture" runs. Today the only backend is
//! an emulated VM (e.g., QEMU TCG), booted with a given hardware profile, with
//! artifacts shared in via virtio-fs and a command run inside it. In the
//! future other backends (e.g., a remote machine) can satisfy the same
//! profile. Console output streams to the host in real time.
//!
//! This crate is backend-agnostic: profiles define the platform requirements,
//! and incubator backends (currently QEMU TCG) satisfy them.

mod profile;
mod qemu;
mod run;

pub use profile::IncubatorProfile;
pub use run::IncubatorConfig;
pub use run::IncubatorOutput;
pub use run::run_in_incubator;
