// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The VM-mutation gate.
//!
//! Operations that mutate the VM are admitted through this gate. Topology
//! operations (hot add/remove of devices and resources) are independent of one
//! another and share the gate; pause, resume, and save are exclusive and
//! exclude everything else. The gate never queues: a conflicting caller is
//! rejected immediately with `ABORTED`. Teardown and quit are terminal and
//! don't use the gate: the service cancels in-flight topology operations
//! instead.
//!
//! The gate must be acquired before any side effect of the operation, and is
//! released when its guard drops, so completion, error, and cancellation all
//! release it. The state is behind `Arc<Mutex<_>>` because guards move into
//! spawned operation futures, which must be `Send`.

use anyhow::Context;
use mesh_rpc::service::Code;
use parking_lot::Mutex;
use std::sync::Arc;

/// An operation that mutates the VM and therefore requires the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VmOperation {
    /// Pause. Exclusive.
    Pause,
    /// Resume. Exclusive.
    Resume,
    /// Save a snapshot. Exclusive.
    Save,
    /// Hot add or remove of a device or resource. Shared with other topology
    /// operations.
    Topology,
}

impl VmOperation {
    fn name(self) -> &'static str {
        match self {
            VmOperation::Pause => "pause",
            VmOperation::Resume => "resume",
            VmOperation::Save => "save",
            VmOperation::Topology => "device or resource modification",
        }
    }

    fn is_shared(self) -> bool {
        matches!(self, VmOperation::Topology)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// `n` topology operations in flight.
    Shared(u32),
    Exclusive(VmOperation),
}

pub(crate) struct VmOperationGate {
    state: Arc<Mutex<State>>,
}

impl Default for VmOperationGate {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Idle)),
        }
    }
}

/// Held for the duration of a gated operation; releases the gate on drop.
#[must_use = "the gate is released as soon as the guard is dropped"]
pub(crate) struct VmOperationGuard {
    state: Arc<Mutex<State>>,
    operation: VmOperation,
}

impl VmOperationGate {
    /// Acquires the gate for `operation`, or fails with `ABORTED` if a
    /// conflicting operation holds it.
    pub(crate) fn try_acquire(&self, operation: VmOperation) -> anyhow::Result<VmOperationGuard> {
        let mut state = self.state.lock();
        let held = match (*state, operation.is_shared()) {
            (State::Idle, true) => {
                *state = State::Shared(1);
                None
            }
            (State::Idle, false) => {
                *state = State::Exclusive(operation);
                None
            }
            (State::Shared(n), true) => {
                // Bounded by the in-flight RPC tasks, far below `u32::MAX`.
                *state = State::Shared(n + 1);
                None
            }
            (State::Shared(_), false) => Some(VmOperation::Topology),
            (State::Exclusive(held), _) => Some(held),
        };
        if let Some(held) = held {
            return Err(anyhow::Error::new(Code::Aborted)).with_context(|| {
                format!(
                    "VM busy: {} in progress, cannot start {}",
                    held.name(),
                    operation.name()
                )
            });
        }
        Ok(VmOperationGuard {
            state: self.state.clone(),
            operation,
        })
    }
}

impl Drop for VmOperationGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        *state = match *state {
            State::Shared(n) if self.operation.is_shared() && n > 1 => State::Shared(n - 1),
            _ => State::Idle,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ttrpc::grpc_error;
    use test_with_tracing::test;

    #[test]
    fn exclusive_rejects_everything_with_aborted() {
        let gate = VmOperationGate::default();
        let _guard = gate.try_acquire(VmOperation::Pause).unwrap();
        for op in [
            VmOperation::Pause,
            VmOperation::Resume,
            VmOperation::Save,
            VmOperation::Topology,
        ] {
            let status = grpc_error(gate.try_acquire(op).err().unwrap());
            assert_eq!(status.code, i32::from(Code::Aborted));
            assert!(
                status.message.contains(&format!(
                    "VM busy: pause in progress, cannot start {}",
                    op.name()
                )),
                "{}",
                status.message
            );
        }
    }

    #[test]
    fn topology_operations_share_the_gate() {
        let gate = VmOperationGate::default();
        let a = gate.try_acquire(VmOperation::Topology).unwrap();
        let b = gate.try_acquire(VmOperation::Topology).unwrap();
        assert!(gate.try_acquire(VmOperation::Pause).is_err());
        drop(a);
        assert!(gate.try_acquire(VmOperation::Resume).is_err());
        drop(b);
        drop(gate.try_acquire(VmOperation::Resume).unwrap());
        drop(gate.try_acquire(VmOperation::Topology).unwrap());
    }

    #[test]
    fn releases_when_holding_future_is_dropped() {
        let gate = VmOperationGate::default();
        let guard = gate.try_acquire(VmOperation::Topology).unwrap();
        // A spawned operation moves the guard into its future; dropping the
        // unpolled future (cancellation) must release the gate.
        let fut = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        assert!(gate.try_acquire(VmOperation::Pause).is_err());
        drop(fut);
        assert!(gate.try_acquire(VmOperation::Pause).is_ok());
    }
}
