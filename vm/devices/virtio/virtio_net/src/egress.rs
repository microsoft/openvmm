// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress policy enforcement for virtio-net transmit.
//!
//! [`NicBuilder::egress_policy`] configures a run-scoped [`EgressPolicy`]. The
//! device validates the policy and installs it on the endpoint, which applies
//! it to the bytes it transmits, and checks the prefix of each guest frame
//! against the policy before handing the frame to the endpoint.

use crate::NicBuilder;
use crate::TxPacketError;
use crate::Worker;
use anyhow::Context as _;
use net_backend::Endpoint;
use net_backend_resources::egress::EgressPolicy;

impl NicBuilder {
    pub fn egress_policy(mut self, egress_policy: EgressPolicy) -> Self {
        self.egress_policy = Some(egress_policy);
        self
    }

    /// Validates the configured egress policy and installs it on `endpoint`.
    pub(crate) fn configure_egress(
        &self,
        mut endpoint: Box<dyn Endpoint>,
    ) -> anyhow::Result<Box<dyn Endpoint>> {
        if let Some(policy) = &self.egress_policy {
            policy
                .validate()
                .context("network device received an invalid egress policy")?;
            endpoint
                .set_egress_policy(policy.clone())
                .with_context(|| {
                    format!(
                        "network backend '{}' rejected egress policy",
                        endpoint.endpoint_type()
                    )
                })?;
        }
        Ok(endpoint)
    }
}

impl Worker {
    /// Checks the prefix of a guest TX frame against the egress policy.
    pub(crate) fn authorize_egress(
        &self,
        packet_prefix: &[u8],
        packet_len: u32,
    ) -> Result<(), TxPacketError> {
        if let Some(policy) = &self.egress_policy {
            policy
                .authorize_frame(packet_prefix, packet_len as usize)
                .map_err(TxPacketError::EgressDenied)?;
        }
        Ok(())
    }
}
