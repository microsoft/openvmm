// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(minimal_rt, no_std, no_main)]

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
//! # Processor Startup
//!
//! The sidecar kernel is initialized by a single bootstrap processor (BSP),
//! which is typically VP 0. This initialization happens during the boot shim
//! phase, before the Linux kernel starts. The BSP (which will later become a
//! Linux CPU) calls into the sidecar kernel to perform all global initialization
//! tasks: copying the hypercall page, setting up the IDT, initializing control
//! pages for each node, and preparing page tables and per-CPU state for all
//! application processors (APs).
//!
//! After the BSP completes its initialization work, it begins starting APs. The
//! startup process uses a fan-out pattern to minimize total boot time: the BSP
//! starts the first few APs, and then each newly-started AP immediately helps
//! start additional APs. This creates an exponential growth in the number of
//! CPUs actively participating in the boot process.
//!
//! Concurrency during startup is managed through atomic operations on a
//! per-node `next_vp` counter. Each CPU (whether BSP or AP) atomically
//! increments this counter to claim the next VP index to start within that
//! node. This ensures that each VP is started exactly once without requiring
//! locks or complex coordination. The startup fan-out continues until all VPs
//! in all nodes have been started (or skipped if marked as REMOVED).
//!
//! Note that the first VP in each NUMA node is typically reserved for the Linux
//! kernel and does not run the sidecar kernel. The sidecar startup logic
//! accounts for this by initializing the `next_vp` counter to 1 for each node,
//! effectively skipping the base VP (index 0) of that node.
//!
//! Each CPU's page tables include a mapping for its node's control page at a
//! fixed virtual address (PTE_CONTROL_PAGE). This is set up during AP
//! initialization via the `init_ap` function, which builds the per-CPU page
//! table hierarchy and maps the control page at the same virtual address for
//! all CPUs in the node. This allows each CPU to access its control page
//! without knowing its physical address, and ensures that all CPUs in a node
//! see the same control page data (since they all map the same physical page).
//! The control page mapping is read-write from the sidecar's perspective, as
//! the sidecar needs to update status fields (like `cpu_status` and
//! `needs_attention`) using atomic operations.
//!
//! As of this writing, sidecar only supports x86_64, without hardware
//! isolation.

mod arch;

#[cfg(not(minimal_rt))]
fn main() {
    panic!("must build with MINIMAL_RT_BUILD=1")
}
