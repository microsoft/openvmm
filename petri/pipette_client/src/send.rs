// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A thin wrapper around a `mesh::Sender<PipetteRequest>` that provides
//! useful error handling semantics.

use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh::CancelContext;
use pipette_protocol::PipetteRequest;
use std::time::Duration;

pub(crate) struct PipetteSender(mesh::Sender<PipetteRequest>);

impl PipetteSender {
    pub(crate) fn new(sender: mesh::Sender<PipetteRequest>) -> Self {
        Self(sender)
    }

    /// A wrapper around [`mesh::Sender::call`] that will sleep for 5 seconds on failure,
    /// allowing any additional work occurring on the system to hopefully complete.
    /// See also [`petri::PetriVm::wait_for_halt_or`]
    pub(crate) async fn call<F, I, R>(&self, f: F, input: I) -> Result<R, mesh::RecvError>
    where
        F: FnOnce(Rpc<I, R>) -> PipetteRequest,
        R: 'static + Send,
    {
        let (result_send, result_recv) = mesh::oneshot();
        self.0.send_rpc(f(Rpc(input, result_send)));
        let result = result_recv.await;
        if result.is_err() {
            tracing::warn!("Pipette request channel failed, sleeping for 5 seconds to let outstanding work finish");
            let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
            let _ = c.cancelled().await;
        }
        result
    }
}
