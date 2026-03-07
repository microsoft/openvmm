// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use mesh::rpc::Rpc;
use netvsp::VirtualFunction;
use parking_lot::Mutex;

/// A fuzzer-controlled [`VirtualFunction`] implementation.
///
/// The fuzzer can control:
/// - What `id()` returns (via `id_send`)
/// - When `wait_for_state_change()` completes (via `state_change_send`)
pub struct FuzzVirtualFunction {
    id_recv: Mutex<mesh::Receiver<Option<u32>>>,
    state_change_recv: mesh::Receiver<Rpc<(), ()>>,
    current_id: Mutex<Option<u32>>,
}

/// Handles for controlling a [`FuzzVirtualFunction`] from the fuzz loop.
pub struct FuzzVfHandles {
    /// Send VF ID updates. The VF will return the latest value from `id()`.
    pub id_send: mesh::Sender<Option<u32>>,
    /// Send an RPC to trigger `wait_for_state_change()` completion.
    pub state_change_send: mesh::Sender<Rpc<(), ()>>,
}

impl FuzzVirtualFunction {
    /// Create a new fuzz VF with an initial ID and its control handles.
    pub fn new(initial_id: Option<u32>) -> (Self, FuzzVfHandles) {
        let (id_send, id_recv) = mesh::channel();
        let (state_change_send, state_change_recv) = mesh::channel();
        (
            Self {
                id_recv: Mutex::new(id_recv),
                state_change_recv,
                current_id: Mutex::new(initial_id),
            },
            FuzzVfHandles {
                id_send,
                state_change_send,
            },
        )
    }

    /// Drain any pending ID updates from the channel, keeping only the latest.
    fn drain_id_updates(&self) {
        let mut latest = None;
        {
            let mut id_recv = self.id_recv.lock();
            while let Ok(id) = id_recv.try_recv() {
                latest = Some(id);
            }
        }
        if let Some(id) = latest {
            *self.current_id.lock() = id;
        }
    }
}

#[async_trait]
impl VirtualFunction for FuzzVirtualFunction {
    async fn id(&self) -> Option<u32> {
        self.drain_id_updates();
        *self.current_id.lock()
    }

    async fn guest_ready_for_device(&mut self) {
        // No-op: the fuzzer controls state changes via the channel.
    }

    async fn wait_for_state_change(&mut self) -> Rpc<(), ()> {
        // Wait for the fuzzer to signal a state change.
        // If the channel is closed (all senders dropped), pend forever
        // so that the coordinator's `until_stopped` can observe the stop
        // signal instead of spinning in a tight loop.
        let rpc = match self.state_change_recv.recv().await {
            Ok(rpc) => rpc,
            Err(_) => std::future::pending().await,
        };
        self.drain_id_updates();
        // Return a real RPC provided by the fuzz loop so the coordinator's
        // `rpc.handle(...)` path performs an actual completion.
        rpc
    }
}
