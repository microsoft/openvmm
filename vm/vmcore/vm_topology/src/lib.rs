// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for describing VM topology (processor packages and layout, memory
//! layout).

#![forbid(unsafe_code)]

pub mod cxl;
pub mod layout;
pub mod memory;
pub mod pcie;
pub mod processor;
