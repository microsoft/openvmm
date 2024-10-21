// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate contains utilities to interact with Flattened DeviceTree binary
//! blobs. Included is a builder and parser, both available as no_std.

#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod builder;
pub mod parser;
mod spec;

pub use spec::ReserveEntry;
