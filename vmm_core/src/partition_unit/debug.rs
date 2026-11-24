// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "gdb")]

//! Implements debugging support for [`PartitionUnitRunner`]. This is a separate
//! module because it is cfg gated on the `gdb` feature.

use super::PartitionUnitRunner;
use anyhow::Context;
use futures::StreamExt;
use guestmem::GuestMemory;
use virt::VpIndex;
use vmm_core_defs::HaltReason;
use vmm_core_defs::debug_rpc::DebugRequest;
use vmm_core_defs::debug_rpc::DebugStopReason;
use vmm_core_defs::debug_rpc::GuestAddress;

pub struct DebuggerState {
    guest_memory: GuestMemory,
    debug_notify_halt: Option<mesh::OneshotSender<DebugStopReason>>,
    rpc: Option<mesh::Receiver<DebugRequest>>,
    attached: bool,
    halt_reported: bool,
    /// Whether the debugger holds the VPs stopped without a halt reason, as it
    /// does from a break into the stopped unit until it resumes them.
    holds_vps: bool,
}

impl DebuggerState {
    pub fn new(guest_memory: GuestMemory, rpc: Option<mesh::Receiver<DebugRequest>>) -> Self {
        Self {
            guest_memory,
            debug_notify_halt: None,
            rpc,
            attached: false,
            halt_reported: false,
            holds_vps: false,
        }
    }

    /// Returns whether the debugger holds the VPs stopped.
    pub fn holds_vps(&self) -> bool {
        self.holds_vps
    }

    /// Ends the debugger's hold on the VPs, returning whether it held them.
    pub fn release_vps(&mut self) -> bool {
        std::mem::take(&mut self.holds_vps)
    }

    pub async fn wait_rpc(&mut self) -> DebugRequest {
        if let Some(rpc) = &mut self.rpc {
            if !futures::stream::FusedStream::is_terminated(&rpc) {
                return rpc.select_next_some().await;
            }
        }
        std::future::pending().await
    }

    /// Returns true if the reason was reported to the debugger.
    pub fn report_halt_to_debugger(&mut self, reason: &HaltReason) -> bool {
        if !self.attached {
            return false;
        }
        if let Some(notify) = self.debug_notify_halt.take() {
            tracing::debug!("halt reported to debugger");
            self.halt_reported = true;
            notify.send(match reason {
                HaltReason::PowerOff
                | HaltReason::PowerOffWithStatus { .. }
                | HaltReason::Hibernate => DebugStopReason::PowerOff,
                HaltReason::Reset => DebugStopReason::Reset,
                HaltReason::Watchdog => DebugStopReason::Watchdog,
                HaltReason::TripleFault { vp, .. } => DebugStopReason::TripleFault { vp: *vp },
                HaltReason::DebugBreak { .. } => DebugStopReason::Break,
                HaltReason::SingleStep { vp } => DebugStopReason::SingleStep { vp: *vp },
                HaltReason::HwBreakpoint { vp, breakpoint } => DebugStopReason::HwBreakpoint {
                    vp: *vp,
                    breakpoint: *breakpoint,
                },
            });
        }
        true
    }
}

impl PartitionUnitRunner {
    pub async fn handle_gdb(&mut self, req: DebugRequest) {
        match req {
            DebugRequest::Attach => {
                tracing::info!("debugger attached");
                self.debugger_state.attached = true;
            }
            DebugRequest::Detach => {
                tracing::info!("debugger detached");
                self.debugger_state.debug_notify_halt = None;
                if let Err(err) = self.vp_set.clear_debug_state().await {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "failed to clear debug state"
                    );
                }
                // Report the halt reason to the client since the debugger won't
                // be there to get it.
                if let Some(reason) = self.halt_reason.clone() {
                    self.client_notify_send.send(reason);
                }
                self.debugger_state.attached = false;
                if self.debugger_state.release_vps() {
                    self.try_start();
                }
            }
            DebugRequest::Resume { response } => {
                tracing::debug!("debugger resumed");
                self.debugger_state.debug_notify_halt = Some(response);
                let held = self.debugger_state.release_vps();
                if self.debugger_state.halt_reported {
                    self.clear_halt();
                    self.debugger_state.halt_reported = false;
                } else if let Some(reason) = self.halt_reason.as_ref() {
                    self.debugger_state.report_halt_to_debugger(reason);
                }
                if held {
                    self.try_start();
                }
            }
            DebugRequest::Break => {
                tracing::debug!("debug break requested");
                if self.debugger_state.holds_vps {
                    // The VPs are already stopped for the debugger.
                } else if self.unit_started {
                    self.vp_set.halt(HaltReason::DebugBreak { vp: None });
                } else if self.halt_reason.is_none() && self.debugger_state.attached {
                    // The VPs are already stopped, and cannot run before the
                    // unit starts, so report the stop that the debugger waits
                    // for at once, and hold the VPs for the debugger until it
                    // resumes them. A halt reason would change the saved state
                    // although the VM has not run.
                    tracing::debug!("debugger holds the stopped VPs");
                    self.debugger_state.holds_vps = true;
                    if let Some(notify) = self.debugger_state.debug_notify_halt.take() {
                        notify.send(DebugStopReason::Break);
                    }
                }
            }
            DebugRequest::SetDebugState { vp, state } => {
                if let Err(err) = self.vp_set.set_debug_state(VpIndex::new(vp), state).await {
                    tracing::error!(
                        vp,
                        error = err.as_ref() as &dyn std::error::Error,
                        "failed to set debug state"
                    );
                }
            }
            DebugRequest::GetVpState(rpc) => {
                rpc.handle_failable(async |vp| self.vp_set.get_vp_state(VpIndex::new(vp)).await)
                    .await
            }
            DebugRequest::SetVpState(rpc) => {
                rpc.handle_failable(async |(vp, state)| {
                    self.vp_set.set_vp_state(VpIndex::new(vp), state).await
                })
                .await
            }
            DebugRequest::ReadMemory(rpc) => {
                rpc.handle_failable(async |(addr, len)| match addr {
                    GuestAddress::Gva { vp, gva } => {
                        self.vp_set
                            .read_virtual_memory(VpIndex::new(vp), gva, len)
                            .await
                    }
                    GuestAddress::Gpa(gpa) => {
                        let mut buf = vec![0; len];
                        self.debugger_state
                            .guest_memory
                            .read_at(gpa, &mut buf)
                            .context("failed to read guest memory")?;
                        Ok(buf)
                    }
                })
                .await
            }
            DebugRequest::WriteMemory(rpc) => {
                rpc.handle_failable(async |(addr, data)| match addr {
                    GuestAddress::Gva { vp, gva } => {
                        self.vp_set
                            .write_virtual_memory(VpIndex::new(vp), gva, data)
                            .await
                    }
                    GuestAddress::Gpa(gpa) => self
                        .debugger_state
                        .guest_memory
                        .write_at(gpa, &data)
                        .context("failed to write guest memory"),
                })
                .await
            }
        }
    }
}
