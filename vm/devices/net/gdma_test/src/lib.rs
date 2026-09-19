// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A GDMA device emulator with out-of-band EQE injection support.
//!
//! The production [`gdma`] crate has no test-specific state;
//! this crate provides the test-controllable variant via
//! [`gdma_resources::GdmaTestDeviceHandle`] and
//! [`resolver::GdmaTestDeviceResolver`].

#![forbid(unsafe_code)]

pub mod resolver;
