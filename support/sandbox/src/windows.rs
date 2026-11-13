// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Windows backend for [`apply`](crate::apply) and
//! [`tighten`](crate::tighten).
//!
//! # Status: deferred
//!
//! [`prepare`](crate::prepare) returns inert
//! [`WindowsPreparation`](crate::WindowsPreparation) data describing an intended
//! LPAC configuration. A Windows launcher would need to realize it against
//! `CreateProcess`; PAL's sandboxed Windows launch is not implemented.
//!
//! Worker-side confinement is also unimplemented. Launch data alone must not
//! be interpreted as an enforced Windows security boundary.
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
