// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A source of information for Underhill confidentiality configuration.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
mod getters;
#[cfg(feature = "std")]
pub use getters::*;

/// The name of the environment variable that indicates whether the current VM is a confidential VM.
pub const OPENHCL_CONFIDENTIAL_ENV_VAR_NAME: &str = "OPENHCL_CONFIDENTIAL";

/// The legacy name of the environment variable that indicates whether the
/// current VM is a confidential VM.
pub const LEGACY_CONFIDENTIAL_ENV_VAR_NAME: &str = "UNDERHILL_CONFIDENTIAL";

/// The name of the environment variable that indicates whether confidential debugging is enabled.
pub const OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME: &str = "OPENHCL_CONFIDENTIAL_DEBUG";

/// The legacy name of the environment variable that indicates whether
/// confidential debugging is enabled.
pub const LEGACY_CONFIDENTIAL_DEBUG_ENV_VAR_NAME: &str = "UNDERHILL_CONFIDENTIAL_DEBUG";
