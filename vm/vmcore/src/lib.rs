// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate contains core virtualization definitions and functionality shared
//! by other virtualization crates (such as VmBus). It is intended to be usable
//! across both HvLite and Hyper-V, so it should not contain any references to
//! HvLite-specific infrastructure (such as WHP).

// UNSAFETY: linkme uses link_section which is unsafe.
#![expect(unsafe_code)]
#![expect(missing_docs)]

// Needed for `save_restore_derive`.
extern crate self as vmcore;

pub mod device_state;
pub mod interrupt;
pub mod isa_dma_channel;
pub mod line_interrupt;
pub mod local_only;
pub mod monitor;
pub mod non_volatile_store;
pub mod notify;
pub mod reference_time_source;
pub mod save_restore;
pub mod slim_event;
pub mod synic;
pub mod vm_task;
pub mod vmtime;
pub mod vpci_msi;
