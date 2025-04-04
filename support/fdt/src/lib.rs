// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate contains utilities to interact with Flattened DeviceTree binary
//! blobs. Included is a builder and parser, both available as no_std.

#![no_std]
#![forbid(unsafe_code)]

pub mod builder;
pub mod parser;
mod spec;

pub use spec::ReserveEntry;
