// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator: launches a controlled environment in which to run petri test
//! commands.
//!
//! An incubator is the place a test "culture" runs. Today the only backend is
//! an emulated VM (e.g., QEMU TCG), booted with a given hardware profile, with
//! artifacts shared in via virtio-9p and a command run inside it. In the
//! future other backends (e.g., a remote machine) can satisfy the same
//! profile. Console output streams to the host in real time.
//!
//! This crate is backend-agnostic: profiles define the platform requirements,
//! and incubator backends (currently QEMU TCG) satisfy them.
//!
//! # Why QEMU rather than OpenVMM?
//!
//! The incubator is fundamentally about providing a *stable host* for running
//! tests against obscure or emulated hardware (unusual IOMMUs, PCIe topologies,
//! device-assignment paths, etc.). QEMU TCG is better at faithfully emulating
//! that breadth of hardware than OpenVMM is, and is likely to remain so. There
//! is intentionally no OpenVMM backend: testing OpenVMM's own nested-virt
//! behavior (where the *outer* VMM is under test) is a separate concern handled
//! elsewhere, not by this crate.

#![forbid(unsafe_code)]

mod profile;
mod qemu;
mod run;

pub use profile::Arch;
pub use profile::IncubatorProfile;
pub use run::IncubatorConfig;
pub use run::IncubatorOutput;
pub use run::run_in_incubator;
