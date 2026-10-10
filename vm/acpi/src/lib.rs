// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for dynamically creating ACPI tables.

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

mod aml;
pub mod builder;
pub mod cedt;
pub mod dsdt;
pub mod slit;
pub mod ssdt;
