// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the VMGS file format.
//!
//! # Implementation Notes
//!
//! This particular implementation of the VMGS file format began life as a
//! line-by-line port of the existing C++ VMGS code in Hyper-V. This kind of
//! rote-porting was fairly common in the early days of the OpenVMM project (when
//! folks were still getting a feel for Rust), though as time has gone on, most
//! instances of rote-ported code have been refactored/rewritten to follow
//! idiomatic Rust patterns.
//!
//! Unfortunately, the VMGS code is pretty complex, and giving it a proper "deep
//! clean" would require a non-trivial amount of developer effort, which as is
//! often the case - not particularly easy to come by.
//!
//! Sure, there's been lots of _incremental_ improvements to the code over the
//! years, and the implementation is in _much_ better shape today than it was in
//! its early days... but the code still has plenty of echoes from that initial
//! C++ port, which really ought to get ironed out.

mod encrypt;
mod error;
mod storage;
mod vmgs_impl;

pub use error::Error;
pub use vmgs_format::EncryptionAlgorithm;
pub use vmgs_format::FileId;
pub use vmgs_impl::GspType;
pub use vmgs_impl::Vmgs;
pub use vmgs_impl::VmgsFileInfo;
#[cfg(feature = "save_restore")]
pub use vmgs_impl::save_restore;

/// VMGS helper functions
pub mod vmgs_helpers {
    pub use crate::vmgs_impl::get_active_header;
    pub use crate::vmgs_impl::read_headers;
    pub use crate::vmgs_impl::validate_header;
}

/// Expose the `VmgsLogger` trait
pub mod logger;
