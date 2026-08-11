// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The build-time-patchable config region.
//!
//! This lives in the test binary rather than in `opentmk_core` on purpose: a
//! `#[used]` static in a library can be dropped when the linker never pulls its
//! object out of the archive, which would leave the section missing and every
//! run failing to patch.

use opentmk_protocol::OpenTmkConfig;

/// The embedded config region, patched in place by host tooling to select the
/// test to run. Layout and parsing live in [`opentmk_protocol`].
// SAFETY: `OPENTMK_CONFIG` is unique, so `no_mangle` cannot collide.
// `link_section = ".tmkcfg"` gives it a dedicated section with the patcher layout.
#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".tmkcfg")]
pub static OPENTMK_CONFIG: OpenTmkConfig = OpenTmkConfig::new();
