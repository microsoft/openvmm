// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for loading guest firmware, available as no_std if no features are defined.

#![no_std]

pub mod linux;
pub mod paravisor;
pub mod shim;
