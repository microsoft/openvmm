// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm CCA specific definitions, including for the Realm Service Interface (RSI).
// TODO: CCA: A lot of the code in this module depends on who gets to package the RSI calls.
// If OpenVMM is the one that packages the RSI calls, then this module should be
// responsible for defining the RSI calls and their parameters. If the kernel driver is the one
// that packages the RSI calls, then this module should only define the data structures used
// to communicate with the kernel driver, and the RSI calls should be defined in the kernel driver.

/// CCA memory permission index, used to set and get Stage 2 memory access permissions
/// via the RSI interface.
#[allow(missing_docs)]
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CcaMemPermIndex {
    Index0,
    Index1,
    Index2,
    Index3,
    Index4,
    Index5,
    Index6,
    Index7,
    Index8,
    Index9,
    Index10,
    Index11,
    Index12,
    Index13,
    #[default]
    Index14,
}
