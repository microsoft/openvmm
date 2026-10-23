// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Base and worker-specific sandbox profiles.
//!
//! Each base is exposed as a function returning a widening [`Builder`], so a
//! worker starts from one and chains its own grants before calling
//! [`Builder::build`]:
//!
//! ```
//! use sandbox::{Network, profiles};
//!
//! let profile = profiles::minimal()
//!     .name("my_worker")
//!     .network(Network::Loopback)
//!     .build();
//! ```
//!
//! Each worker's sandbox policy is defined in its own module.
//! [`SandboxRole::profile`] builds the policy defined by the selected worker's module:
//!
//! ```
//! use sandbox::profiles::SandboxRole;
//!
//! let profile = SandboxRole::Tpm.profile();
//! assert_eq!(profile.name(), "tpm_worker");
//! ```
//!
//! [`Builder`]: crate::Builder
//! [`Builder::build`]: crate::Builder::build

mod minimal;
mod sandbox_profiles;
mod tpm_worker;
mod vm_worker;

pub use minimal::minimal;
pub use sandbox_profiles::SandboxRole;
pub use tpm_worker::tpm_worker;
pub use vm_worker::vm_worker;
