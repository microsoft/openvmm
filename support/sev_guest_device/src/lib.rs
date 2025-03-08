// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate includes the abstraction layer of Linux SEV-SNP Guest APIs and
//! definitions of data structures according to SEV-SNP specification.


pub mod protocol;

#[cfg(target_os = "linux")]
pub mod ioctl;
