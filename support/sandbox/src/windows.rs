// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Windows backend for [`apply`](crate::apply) and
//! [`tighten`](crate::tighten).
//!
//! # Status: deferred
//!
//! On Windows the security-critical work is the LPAC construction, which
//! happens *before* the child exists. This crate already surfaces that portion
//! as pure data: [`prepare`](crate::prepare) returns a
//! [`WindowsPreparation`](crate::WindowsPreparation) describing the AppContainer
//! moniker, capability monikers, and launch-time mitigation flags, and the
//! caller realizes it against `CreateProcess`.
//!
//! What remains for Windows is the *post-launch* half of `apply` — the
//! mitigations a worker must self-apply after creation (Win32k lockdown, child
//! process policy, token privilege strip, Job Object). That half needs
//! `windows-sys` and a Windows host to build and validate against, neither of
//! which is available in this environment, so it is intentionally left
//! unimplemented rather than stubbed with an untested no-op that would give a
//! false sense of security (R-S12: never degrade silently).
//!
//! Until it lands, [`apply`] and [`tighten`] return
//! [`Error::UnsupportedPlatform`] on Windows so a caller wiring this crate onto
//! the Windows worker surface fails loudly instead of running unconfined.

use crate::Error;
use crate::profile::Profile;
use crate::profile::Restrictions;

/// STAGE 2 (Windows) — deferred. Returns [`Error::UnsupportedPlatform`].
///
/// See the module docs: the launch-time LPAC portion is delivered as data via
/// [`prepare`](crate::prepare); the post-launch mitigation half is not yet
/// implemented.
pub fn apply(profile: &Profile) -> Result<(), Error> {
    let _ = profile;
    Err(Error::UnsupportedPlatform)
}

/// STAGE 3 (Windows) — deferred. Returns [`Error::UnsupportedPlatform`].
pub fn tighten(restrictions: &Restrictions) -> Result<(), Error> {
    let _ = restrictions;
    Err(Error::UnsupportedPlatform)
}
