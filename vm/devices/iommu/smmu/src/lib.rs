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

pub use emulator::SmmuConfig;
pub use emulator::SmmuDevice;
pub use shared::SmmuSharedState;
pub use shared::SmmuSignalMsi;
pub use shared::SmmuTranslator;
