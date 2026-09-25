// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How a VM launch configures the REPL beyond the resources it shares with it:
//! whether the REPL reads stdin ([`super::headless`] otherwise), and whether a
//! paused snapshot restore awaits its first resume ([`super::restore_ready`]).

use crate::Options;

/// How a VM launch configures the REPL.
pub(crate) struct ReplLaunch {
    /// Whether a paused snapshot restore waits for its first successful
    /// `resume`, which publishes its restore readiness event.
    pub(crate) restore_ready_pending: bool,
    /// Whether the REPL reads stdin. Otherwise, it runs headless.
    pub(crate) stdin_enabled: bool,
}

impl ReplLaunch {
    /// Returns how a VM launched from the command line configures the REPL.
    pub(crate) fn from_options(opt: &Options) -> Self {
        Self {
            restore_ready_pending: opt.paused && opt.restore_ready_path.is_some(),
            stdin_enabled: !opt.microvm.microvm_control_auth_stdin,
        }
    }
}
