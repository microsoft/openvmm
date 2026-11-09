// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Policy vocabulary and control-side preparation for worker sandboxing.
//!
//! A [`Profile`] records filesystem grants, network scope, and syscall denials.
//! [`prepare`] translates launch requirements into inert data for a process
//! builder. Neither operation creates a process or confines the caller.
//! Worker-side enforcement and process-launch integration are separate layers.

#![warn(missing_docs)]

mod prepare;
mod profile;

pub use prepare::HandleTag;
pub use prepare::Identity;
pub use prepare::RawHandle;
pub use prepare::SandboxProcessConfig;
pub use prepare::WindowsPreparation;
pub use prepare::prepare;
pub use profile::Builder;
pub use profile::Capability;
pub use profile::Network;
pub use profile::Profile;
pub use profile::Restrictions;
pub use profile::RestrictionsBuilder;
pub use profile::Syscalls;

/// Process exit code reserved for required sandbox setup failures.
pub const EXIT_SANDBOX_FAILED: i32 = 90;

/// Environment variable disabling sandbox preparation in debug builds only.
///
/// Release builds ignore this variable.
pub const DISABLE_ENV: &str = "OPENVMM_SANDBOX_DISABLE";

/// An error produced while preparing or applying a sandbox.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The current build has no implemented sandbox backend for this platform.
    #[error("sandbox is not implemented on this platform")]
    UnsupportedPlatform,

    /// A required primitive is unavailable in this environment.
    #[error("required sandbox primitive unavailable: {0}")]
    RequiredPrimitiveUnavailable(&'static str),

    /// A required primitive failed while being applied.
    #[error("required sandbox primitive `{primitive}` failed to apply")]
    ApplyFailed {
        /// The primitive that failed.
        primitive: &'static str,
        /// The underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
}

fn sandbox_disabled() -> bool {
    cfg!(debug_assertions) && std::env::var_os(DISABLE_ENV).is_some()
}
