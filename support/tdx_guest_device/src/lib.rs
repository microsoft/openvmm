// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate includes the abstraction layer of Linux TDX Guest APIs and
//! definitions of data structures according to TDX specification.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod protocol;

#[cfg(target_os = "linux")]
pub mod ioctl;
