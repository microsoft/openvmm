// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for dynamically creating ACPI tables.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod aml;
pub mod builder;
pub mod cedt;
pub mod dsdt;
pub mod ssdt;
