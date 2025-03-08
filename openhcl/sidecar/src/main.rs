// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(minimal_rt, no_std, no_main)]
#![warn(missing_docs)]

//! This crate implements the OpenHCL sidecar kernel. This is a kernel that runs
//! along side the OpenHCL Linux kernel, operating on a subset of the virtual
//! machine's CPUs.
//!
//! This is done to avoid needing to boot all CPUs into Linux, since this is
//! very expensive for large VMs. Instead, most of the CPUs are run in the
//! sidecar kernel, where they run a minimal dispatch loop. If a sidecar CPU
//! hits a condition that it cannot handle locally (e.g., the guest OS attempts
//! to access an emulated device), it will send a message to the main Linux
//! kernel. One of the Linux CPUs can then handle the exit remotely, and/or
//! convert the sidecar CPU to a Linux CPU.
//!
//! Similarly, if a Linux CPU needs to run code on a sidecar CPU (e.g., to run
//! it as a target for device interrupts from the host), it can convert the
//! sidecar CPU to a Linux CPU.
//!
//! Sidecar is modeled to Linux as a set of devices, one per node (a contiguous
//! set of CPUs; this may or may not correspond to a NUMA node or CPU package).
//! Each device has a single control page, used to communicate with the sidecar
//! CPUs. Each CPU additionally has a command page, which is used to specify
//! sidecar commands (e.g., run the VP, or get or set VP registers). These
//! commands are in separate pages at least partially so that they can be
//! operated on independently; the Linux kernel communicates with sidecar via
//! control page, and the user-mode VMM communicates with the individual sidecar
//! CPUs via the command pages.
//!
//! The sidecar kernel is a very simple kernel. It runs at a fixed virtual
//! address (although it is still built with dynamic relocations). Each CPU has
//! its own set of page tables (sharing some portion of them) so that they only
//! map what they use. Each CPU is independent after boot; sidecar CPUs never
//! communicate with each other and only communicate with Linux CPUs, via the
//! Linux sidecar driver.
//!
//! The sidecar CPU runs a simple dispatch loop. It halts the processor, waiting
//! for the control page to indicate that it should run (the sidecar driver
//! sends an IPI when the control page is updated). It then reads a command from
//! the command page and executes the command; if the command can run for an
//! unbounded amount of time (e.g., the command to run the VP), then the driver
//! can interrupt the command via another request on the control page (and
//! another IPI).
//!
//! As of this writing, sidecar only supports x86_64, without hardware
//! isolation.

mod arch;

#[cfg(not(minimal_rt))]
fn main() {
    panic!("must build with MINIMAL_RT_BUILD=1")
}
