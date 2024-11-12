// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Creates a digest for supported isolation types which can be signed externally.

pub mod snp;
pub mod tdx;
pub mod vbs;

pub use snp::generate_snp_measurement;
pub use tdx::generate_tdx_measurement;
pub use vbs::generate_vbs_measurement;

const SHA_256_OUTPUT_SIZE_BYTES: usize = 32;
const SHA_384_OUTPUT_SIZE_BYTES: usize = 48;
