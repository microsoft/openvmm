// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore readiness in the REPL.
//!
//! A VM restored with `--restore-snapshot --restore-ready-path` publishes its
//! restore readiness event, once, when it first starts. With `--paused`, that
//! first start is the REPL's first successful `resume`; until then, the
//! restore readiness is pending (`ReplLaunch::restore_ready_pending`). A
//! resume that fails while it is pending ends the REPL with an error, which
//! tears the restored VM down: the single-use readiness endpoint may already
//! be consumed, and a later resume would start the guest without the event.

use mesh::error::RemoteError;

/// Target of the event logged here: the REPL's own, next to its other resume
/// events.
const REPL_TARGET: &str = "openvmm_entry::repl";

/// Handles a failed `resume` command. The failure is logged; while the
/// restore readiness is `pending`, it also ends the REPL with an error.
pub(super) fn resume_failed(err: RemoteError, pending: bool) -> anyhow::Result<()> {
    tracing::error!(
        target: REPL_TARGET,
        error = &err as &dyn std::error::Error,
        "resume failed"
    );
    if pending {
        return Err(anyhow::Error::new(err).context("restore readiness resume failed"));
    }
    Ok(())
}
