// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 emulator for OpenVMM.
//!
//! This crate implements an Arm SMMUv3 (System Memory Management Unit)
//! emulator, providing IOVA→GPA translation for devices behind the SMMU.

#![forbid(unsafe_code)]

mod emulator;
mod shared;
mod spec;
mod translate;

pub use emulator::HostSmmuCaps;
pub use emulator::SmmuConfig;
pub use emulator::SmmuDevice;
pub use emulator::SmmuOasPolicy;
pub use shared::AccelIommuAssociation;
pub use shared::AccelRegistration;
pub use shared::AcceleratedStreamBackend;
pub use shared::AcceleratedViommu;
pub use shared::SmmuNestingContext;
pub use shared::SmmuSharedState;
pub use shared::SmmuSignalMsi;
pub use shared::SmmuTranslator;
pub use shared::StreamConfig;

/// Valid SMMUv3 output address sizes, in bits (IDR5.OAS encodings).
pub const VALID_OAS_BITS: [u8; 7] = [32, 36, 40, 42, 44, 48, 52];
