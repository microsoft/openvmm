// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Headless operation of the REPL, for runs driven by another process instead
//! of an interactive console.
//!
//! Without stdin (`--microvm-control-auth-stdin` reserves stdin for the
//! control-console capability), the REPL starts no stdio thread and handles
//! only controller events, until one of them ends the run. With a stdin that
//! is not a terminal, such as a pipe, the stdio thread reads it without
//! switching the console to raw mode.

use crate::vm_controller::VmControllerEvent;
use futures::StreamExt;
use std::io;
use std::io::IsTerminal;

/// Target of the events logged here: the REPL's own, so that the headless
/// REPL logs controller events exactly as the interactive one does.
const REPL_TARGET: &str = "openvmm_entry::repl";

/// Runs the REPL without reading stdin.
///
/// No command can arrive, so this handles only the controller events, as the
/// interactive loop of [`super::run_repl`] does, and returns the exit status of
/// the event that ends the run.
pub(super) async fn run(
    vm_controller_events: &mut mesh::Receiver<VmControllerEvent>,
) -> anyhow::Result<i32> {
    while let Some(event) = vm_controller_events.next().await {
        if let Some(code) = controller_exit(event)? {
            return Ok(code);
        }
    }
    // As in the interactive loop, whose other event sources stay open, the
    // controller closing its event channel does not end the run.
    std::future::pending().await
}

/// Maps a controller event to the exit status of the REPL, or to `None` when
/// the REPL keeps running.
fn controller_exit(event: VmControllerEvent) -> anyhow::Result<Option<i32>> {
    match event {
        VmControllerEvent::WorkerStopped { error } => {
            if let Some(err) = &error {
                tracing::error!(target: REPL_TARGET, error = err.as_str(), "vm worker stopped");
            }
            Ok(Some(i32::from(error.is_some())))
        }
        VmControllerEvent::VncWorkerStopped { .. } => Ok(None),
        VmControllerEvent::GuestHalt(reason) => {
            tracing::info!(target: REPL_TARGET, reason = reason.as_str(), "guest halted");
            Ok(None)
        }
        VmControllerEvent::ExitRequested { code } => Ok(Some(code)),
        VmControllerEvent::ExitFailed { error } => Err(anyhow::anyhow!(error)),
    }
}

/// Enables raw console mode for the console input of the stdio thread, unless
/// stdin is not a terminal. Returns whether raw mode was enabled.
pub(super) fn enable_raw_mode(stdin: &io::Stdin) -> io::Result<bool> {
    let terminal = stdin.is_terminal();
    if terminal {
        crossterm::terminal::enable_raw_mode()?;
    }
    Ok(terminal)
}

/// Disables raw console mode if [`enable_raw_mode`] enabled it.
pub(super) fn disable_raw_mode(enabled: bool) -> io::Result<()> {
    if enabled {
        crossterm::terminal::disable_raw_mode()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::controller_exit;
    use crate::repl::ReplResources;
    use crate::repl::launch::ReplLaunch;
    use crate::repl::run_repl;
    use crate::vm_controller::VmControllerEvent;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::pin::pin;
    use test_with_tracing::test;

    #[async_test]
    async fn headless_repl_waits_for_controller_without_stdin(driver: DefaultDriver) {
        let (vm_rpc, _vm_requests) = mesh::channel();
        let (vm_controller, _controller_requests) = mesh::channel();
        let (events, vm_controller_events) = mesh::channel();
        let resources = ReplResources {
            vm_rpc,
            vm_controller,
            vm_controller_events,
            scsi_rpc: None,
            nvme_vtl2_rpc: None,
            consomme_rpc: None,
            shutdown_ic: None,
            kvp_ic: None,
            console_in: None,
            has_vtl2: false,
            launch: ReplLaunch {
                restore_ready_pending: false,
                stdin_enabled: false,
            },
        };
        let mut repl = pin!(run_repl(&driver, resources));
        assert!(futures::poll!(repl.as_mut()).is_pending());
        events.send(VmControllerEvent::ExitRequested { code: 23 });
        assert_eq!(repl.await.unwrap(), 23);
    }

    #[test]
    fn repl_propagates_exit_failures_and_preserves_requested_statuses() {
        let error = controller_exit(VmControllerEvent::ExitFailed {
            error: "console output drain timed out".to_owned(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("console output drain timed out"));
        for code in [0, 37] {
            assert_eq!(
                controller_exit(VmControllerEvent::ExitRequested { code }).unwrap(),
                Some(code)
            );
        }
    }
}
