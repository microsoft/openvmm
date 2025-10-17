// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express definitions and emulators.

#![forbid(unsafe_code)]

pub mod root;

#[cfg(test)]
mod test_helpers;

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE64: u64 = 4096;
const PAGE_OFFSET_MASK: u64 = PAGE_SIZE64 - 1;
const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();

const VENDOR_ID: u16 = 0x1414;
const ROOT_PORT_DEVICE_ID: u16 = 0xC030;

const MAX_FUNCTIONS_PER_BUS: usize = 256;

const BDF_BUS_SHIFT: u16 = 8;
const BDF_DEVICE_SHIFT: u16 = 3;
const BDF_DEVICE_FUNCTION_MASK: u16 = 0x00FF;
