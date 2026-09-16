// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for dynamically creating ACPI tables.
//!
//! The core builders use `no_std` with `alloc`. The default `cxl` feature adds
//! CEDT support and its hosted device-definition dependency; disable default
//! features for freestanding PCIe ACPI generation.
//!
//! The builders use `alloc` and support `no_std`. The default `cxl` feature
//! enables CEDT generation and its hosted CXL dependencies. Disable default
//! features when building for a freestanding target.

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

mod aml;
pub mod builder;
#[cfg(feature = "cxl")]
pub mod cedt;
pub mod dsdt;
pub mod ssdt;
