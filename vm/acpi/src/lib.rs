// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for dynamically creating ACPI tables.
//!
//! [`minimal`] builds the base tables for a simple platform in `no_std` boot
//! environments.

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

mod aml;
pub mod builder;
pub mod cedt;
pub mod dsdt;
pub mod minimal;
pub mod ssdt;
