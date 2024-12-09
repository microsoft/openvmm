// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate includes the abstraction layer of Linux SEV-SNP Guest APIs and
//! definitions of data structures according to SEV-SNP specification.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]
// UNSAFETY: unsafe needed to make ioctl calls.
#![allow(unsafe_code)]

pub mod protocol;

#[cfg(target_os = "linux")]
pub mod ioctl;
