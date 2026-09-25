// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How a VM launch configures the REPL beyond the resources it shares with it:
//! whether the REPL reads stdin ([`super::headless`] otherwise), whether the
//! end of that stdin ends the run, and whether a paused snapshot restore awaits
//! its first resume ([`super::restore_ready`]).

use crate::Options;
use crate::cli_args::microvm::MachineProfileCli;

/// How a VM launch configures the REPL.
pub(crate) struct ReplLaunch {
    /// Whether a paused snapshot restore waits for its first successful
    /// `resume`, which publishes its restore readiness event.
    pub(crate) restore_ready_pending: bool,
    /// Whether the REPL reads stdin. Otherwise, it runs headless.
    pub(crate) stdin_enabled: bool,
    /// Whether the end of a stdin that is not a terminal ends the run, as the
    /// end of a script of commands does. A microVM instead runs until its
    /// guest stops it, so the end of its stdin ends only its console input.
    pub(crate) stdin_eof_ends_run: bool,
}

impl ReplLaunch {
    /// Returns how a VM launched from the command line configures the REPL.
    pub(crate) fn from_options(opt: &Options) -> Self {
        Self {
            restore_ready_pending: opt.paused && opt.restore_ready_path.is_some(),
            stdin_enabled: !opt.microvm.microvm_control_auth_stdin,
            stdin_eof_ends_run: opt.machine != MachineProfileCli::Microvm,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReplLaunch;
    use crate::Options;
    use clap::Parser;

    #[test]
    fn only_a_microvm_outlasts_its_stdin() {
        let opt = Options::try_parse_from(["openvmm"]).unwrap();
        assert!(ReplLaunch::from_options(&opt).stdin_eof_ends_run);

        let opt = Options::try_parse_from(["openvmm", "--machine", "microvm"]).unwrap();
        assert!(!ReplLaunch::from_options(&opt).stdin_eof_ends_run);
    }
}
