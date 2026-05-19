// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor Saved State builder for `.vmrs` dump files.
//!
//! Constructs partition state blobs (VP registers as hypervisor save/restore
//! chunks) and writes complete `.vmrs` files that WinDbg can open via
//! `VmSavedStateDumpProvider.dll`.
//!
//! # Architecture
//!
//! - [`PartitionStateBuilder`] — builds the partition state chunk stream
//!   (Prolog, VpIndices, per-VP register chunks, Epilog)
//! - [`VmrsWriter`] — assembles a complete `.vmrs` file with partition state,
//!   memory blocks, and metadata keys
//!
//! # Usage
//!
//! ```rust,no_run
//! use hyperv_dump::{PartitionStateBuilder, VmrsWriter, ProcessorArch, VpState, X64VpState};
//! use hvdef::Vtl;
//!
//! # fn example(regs: virt::x86::vp::Registers, debug_regs: virt::x86::vp::DebugRegisters, xsave: virt::x86::vp::Xsave, xcr0: virt::x86::vp::Xcr0) {
//! // Build partition state from VP registers
//! let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
//! builder.set_os_id(0); // unenlightened guest
//! builder.add_vp(0, vec![(Vtl::Vtl0, VpState::X64(X64VpState {
//!     registers: regs,
//!     debug_registers: debug_regs,
//!     xsave,
//!     xcr0,
//! }))]);
//! let blob = builder.finish();
//!
//! // Write complete VMRS file with streaming memory
//! let file = std::fs::File::create("dump.vmrs").unwrap();
//! let mut vmrs = VmrsWriter::new(file).unwrap();
//! vmrs.set_partition_state(blob);
//! vmrs.add_memory_range(memory_range::MemoryRange::new(0..4096));
//! # struct NullReader;
//! # impl hyperv_dump::GuestMemoryReader for NullReader {
//! #     fn read_gpa(&mut self, _: u64, buf: &mut [u8]) -> std::io::Result<()> { buf.fill(0); Ok(()) }
//! # }
//! # let mut mem = NullReader;
//! vmrs.finish(&mut mem).unwrap();
//! # }
//! ```

mod defs;
mod partition_state;
mod vmrs_writer;

pub use partition_state::Aarch64VpState;
pub use partition_state::PartitionStateBuilder;
pub use partition_state::ProcessorArch;
pub use partition_state::VpState;
pub use partition_state::X64VpState;
pub use vmrs_writer::GuestMemoryReader;
pub use vmrs_writer::VmrsWriter;
