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
//! use hv_saved_state::{PartitionStateBuilder, VmrsWriter, ProcessorArch};
//!
//! // Build partition state from VP registers
//! let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
//! builder.set_os_id(0); // unenlightened guest
//! builder.add_x64_vp(0, &Default::default());
//! let blob = builder.finish();
//!
//! // Write complete VMRS file
//! let file = std::fs::File::create("dump.vmrs").unwrap();
//! let mut vmrs = VmrsWriter::new(file).unwrap();
//! vmrs.set_partition_state(blob);
//! vmrs.add_memory_range(0, vec![0u8; 4096]);
//! vmrs.finish().unwrap();
//! ```

mod partition_state;
mod vmrs_writer;

pub use partition_state::Aarch64VpRegisters;
pub use partition_state::PartitionStateBuilder;
pub use partition_state::ProcessorArch;
pub use partition_state::X64VpRegisters;
pub use vmrs_writer::VmrsWriter;
