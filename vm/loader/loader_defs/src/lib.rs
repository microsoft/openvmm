// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for loading guest firmware, available as no_std if no features are defined.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

pub mod linux;
pub mod paravisor;
pub mod shim;
