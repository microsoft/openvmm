// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the protocol between `tmk_vmm` and the test microkernel.

#![no_std]

/// Address for logging. Write `&[gpa, len]: &[u64; 2]` of a UTF-8 string to
/// log.
pub const TMK_ADDRESS_LOG: u64 = 0xffff0000;
/// Address for reporting test completion. Write 0 to signal completion.
pub const TMK_ADDRESS_COMPLETE: u64 = 0xffff0008;
