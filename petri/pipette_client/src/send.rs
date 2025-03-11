// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A thin wrapper around a `mesh::Sender<PipetteRequest>` that provides
//! useful error handling semantics.

use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
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
    /// See also [`petri::PetriVmOpenVmm::wait_for_halt_or`]
    pub(crate) async fn call<F, I, R>(&self, f: F, input: I) -> Result<R, RpcError>
    where
        F: FnOnce(Rpc<I, R>) -> PipetteRequest,
        R: 'static + Send,
    {
        let result = self.0.call(f, input).await;
        if result.is_err() {
            tracing::warn!("Pipette request channel failed, sleeping for 5 seconds to let outstanding work finish");
            let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
            let _ = c.cancelled().await;
        }
        result
    }

    /// A wrapper around [`mesh::Sender::call_failable`] that will sleep for 5 seconds on failure,
    /// allowing any additional work occurring on the system to hopefully complete.
    /// See also [`petri::PetriVmOpenVmm::wait_for_halt_or`]
    pub(crate) async fn call_failable<F, I, T, E>(&self, f: F, input: I) -> Result<T, RpcError<E>>
    where
        F: FnOnce(Rpc<I, Result<T, E>>) -> PipetteRequest,
        T: 'static + Send,
        E: 'static + Send,
    {
        let result = self.0.call_failable(f, input).await;
        if result.is_err() {
            tracing::warn!("Pipette request channel failed, sleeping for 5 seconds to let outstanding work finish");
            let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
            let _ = c.cancelled().await;
        }
        result
    }
}
