// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Default-deny worker-process sandboxing for OpenVMM and OpenHCL.
//!
//! This crate confines a worker process to the minimum authority it needs. A
//! worker starts from [`Profile::deny_all`] and is widened only by the explicit
//! grants its policy declares, so the confinement is default-deny by
//! construction rather than by remembering to lock things down.
//!
//! # Three stages
//!
//! Sandboxing happens in named, ordered stages:
//!
//! 1. [`prepare`] — runs in the *control* process, just before it spawns the
//!    worker. It returns a plain-data [`SandboxProcessConfig`] describing the
//!    child's launch environment (which Linux namespaces to create, which
//!    handles stay inheritable, the child's identity, and — on Windows — the
//!    LPAC construction data). The caller passes that prepared configuration
//!    intact to the platform process builder, which performs the spawn.
//!    This crate deliberately depends on neither `mesh` nor `pal`: it decides
//!    *what* the launch environment must be; PAL decides *how* to apply it.
//! 2. [`apply`] — the **first statement of the worker's `main()`**. On Linux the
//!    worker configures the namespaces it was cloned into, then applies
//!    `pivot_root`, credential drop, hardening, `no_new_privs`, and optional
//!    seccomp. It must run before any thread is spawned, any async runtime
//!    starts, or any resource is opened.
//! 3. [`tighten`] — an optional, additive-only ratchet a worker calls once it
//!    has finished initializing, to shed its init-only authority. It takes
//!    [`Restrictions`], a type that can only ever *narrow*.
//!
//! # Handle hygiene is the process builder's job
//!
//! There is no grant envelope and no descriptor sweep inside this crate.
//! [`prepare`] reports exactly which tagged handles must remain inheritable;
//! the platform process builder keeps those and its configured standard I/O
//! inheritable while closing or marking every other descriptor before the
//! child is reached.
//!
//! # Platform support
//!
//! Linux is fully implemented. On Windows the launch-time LPAC data is produced
//! by [`prepare`], but the post-launch [`apply`]/[`tighten`] half is not yet
//! implemented and returns [`Error::UnsupportedPlatform`] rather than a silent
//! no-op (fail-loud). Other platforms are unsupported.

#![warn(missing_docs)]

mod prepare;
mod profile;
mod syscall_denylist;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod unix;
#[cfg(windows)]
mod windows;

pub mod profiles;

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
pub use syscall_denylist::DeniedSyscall;
pub use syscall_denylist::Enforcement;
pub use syscall_denylist::platform_syscall_denylist;

#[cfg(target_os = "linux")]
pub use unix::seccomp::nr_for_name;

/// Process exit code used when a required sandbox primitive fails to apply.
///
/// A worker whose [`apply`] returns an [`Error`] should exit with this code so
/// the control process can distinguish a sandbox failure from an ordinary
/// worker crash. Distinct and stable so tooling and tests can assert on it.
pub const EXIT_SANDBOX_FAILED: i32 = 90;

/// Environment variable that disables the sandbox — **debug builds only**.
///
/// When this crate is built with `debug_assertions` and this variable is
/// present in the environment, [`apply`] and [`tighten`] become no-ops so a
/// developer can run a worker outside its confinement while iterating. Release
/// builds ignore it entirely: the sandbox can never be switched off by an
/// environment variable a compromised process could set (R-O4).
pub const DISABLE_ENV: &str = "OPENVMM_SANDBOX_DISABLE";

/// An error produced while preparing or applying a sandbox.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The current build has no sandbox backend for this operating system, or
    /// the backend's worker-side half is not yet implemented (Windows).
    /// Returned instead of silently running unconfined.
    #[error("sandbox is not implemented on this platform")]
    UnsupportedPlatform,

    /// A primitive the platform must support to honor the profile is
    /// unavailable in this environment.
    #[error("required sandbox primitive unavailable: {0}")]
    RequiredPrimitiveUnavailable(&'static str),

    /// A required sandbox primitive failed while being applied. Carries the
    /// primitive's name and the underlying OS error.
    #[error("required sandbox primitive `{primitive}` failed to apply")]
    ApplyFailed {
        /// The name of the primitive that failed (e.g. `"user_namespace"`).
        primitive: &'static str,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// STAGE 2 — worker, the first statement of `main()`.
///
/// Applies `profile` to the current process. On Linux the process must already
/// have been cloned with the flags returned by [`prepare`]; this configures
/// those namespaces and establishes the remaining sandbox. See the crate docs
/// for the required single-threaded, run-first ordering. Returns [`Error`] if
/// any *required* primitive fails — the worker should then exit with
/// [`EXIT_SANDBOX_FAILED`].
///
/// In debug builds, becomes a no-op when [`DISABLE_ENV`] is set (see its docs).
pub fn apply(profile: &Profile) -> Result<(), Error> {
    if sandbox_disabled() {
        tracing::warn!(
            profile = profile.name(),
            "sandbox disabled via {DISABLE_ENV}; running unconfined (debug build only)",
        );
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux::apply(profile)
    }
    #[cfg(all(windows, not(target_os = "linux")))]
    {
        windows::apply(profile)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = profile;
        Err(Error::UnsupportedPlatform)
    }
}

/// STAGE 3 (optional) — worker, after its own initialization.
///
/// Stacks the additive-only `restrictions` onto the already-applied sandbox.
/// Unlike [`apply`], it may run multi-threaded: Linux synchronizes syscall
/// filters across all threads with seccomp TSYNC. Returns [`Error`] if
/// synchronization fails or is unsupported, with no thread-local fallback.
/// The worker should then exit with [`EXIT_SANDBOX_FAILED`]; restrictions
/// installed before the failure remain in effect.
///
/// In debug builds, becomes a no-op when [`DISABLE_ENV`] is set.
pub fn tighten(restrictions: &Restrictions) -> Result<(), Error> {
    if sandbox_disabled() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux::tighten(restrictions)
    }
    #[cfg(all(windows, not(target_os = "linux")))]
    {
        windows::tighten(restrictions)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = restrictions;
        Err(Error::UnsupportedPlatform)
    }
}

/// Whether the dev-only disable escape is active. Always `false` in release
/// builds, regardless of the environment.
fn sandbox_disabled() -> bool {
    cfg!(debug_assertions) && std::env::var_os(DISABLE_ENV).is_some()
}
