// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for dynamically creating ACPI tables.
//!
//! The core builders use `no_std` with `alloc`. The default `cxl` feature adds
//! CEDT support and its hosted device-definition dependency; disable default
//! features for freestanding ACPI generation. [`snp`] constructs the fixed
//! x86 SNP Linux base tables from a bounded, validated topology.

#![no_std]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

mod aml;
pub mod builder;
#[cfg(feature = "cxl")]
pub mod cedt;
pub mod dsdt;
pub mod snp;
pub mod ssdt;
