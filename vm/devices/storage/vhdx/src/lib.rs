// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-Rust VHDX file format support.
//!
//! This crate is being built bottom-up. The initial surface contains the
//! on-disk format definitions, checksum helpers, and error taxonomy used by
//! later parsing and I/O layers.

#![forbid(unsafe_code)]

pub mod error;
pub mod format;

pub use error::CreateError;
pub use error::InvalidFormatReason;
pub use error::OpenError;
pub use error::VhdxIoError;
pub use error::VhdxIoErrorKind;
