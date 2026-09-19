// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for [`GdmaTestDeviceHandle`].

use async_trait::async_trait;
use futures::StreamExt;
use gdma::GdmaDevice;
use gdma::resolver::Error;
use gdma::test_helpers::hwc_eq_injector;
use gdma::test_helpers::resolve_vports;
use gdma_defs::EqeVfReset;
use gdma_defs::GDMA_EQE_HWC_RESET_REQUEST;
use gdma_resources::GdmaTestDeviceHandle;
use gdma_resources::GdmaTestRequest;
use pal_async::task::Spawn;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;
use zerocopy::IntoBytes;

/// Resource resolver for [`GdmaTestDeviceHandle`].
///
/// Creates a standard GDMA device and spawns a background task that translates
/// test requests into EQEs injected directly into the HWC EQ via
/// [`gdma::test_helpers::hwc_eq_injector`]. The task exits when test control is
/// shut down.
pub struct GdmaTestDeviceResolver;

declare_static_async_resolver! {
    GdmaTestDeviceResolver,
    (PciDeviceHandleKind, GdmaTestDeviceHandle),
}

enum EncodedTestRequest {
    VfReset(EqeVfReset),
}

impl EncodedTestRequest {
    fn eqe_type(&self) -> u8 {
        match self {
            Self::VfReset(_) => GDMA_EQE_HWC_RESET_REQUEST,
        }
    }

    fn data(&self) -> &[u8] {
        match self {
            Self::VfReset(data) => data.as_bytes(),
        }
    }
}

fn encode_request(request: GdmaTestRequest) -> EncodedTestRequest {
    match request {
        GdmaTestRequest::Shutdown => unreachable!("shutdown requests are handled by the loop"),
        GdmaTestRequest::VfReset { revoke_vtl0_vf } => {
            EncodedTestRequest::VfReset(EqeVfReset::new().with_revoke_vtl0_vf(revoke_vtl0_vf))
        }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, GdmaTestDeviceHandle> for GdmaTestDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: GdmaTestDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let vports = resolve_vports(resolver, resource.vports).await?;

        let device = GdmaDevice::new(
            input.driver_source,
            input.dma_target.guest_memory().clone(),
            input.dma_target.msi_target(),
            vports,
            input.register_mmio,
        );

        let inject_eqe = hwc_eq_injector(&device);
        let mut request_recv = resource.request_recv;

        input
            .driver_source
            .simple()
            .spawn("gdma-test-control", async move {
                while let Some(rpc) = request_recv.next().await {
                    let mut shutdown = false;
                    rpc.handle(async |request| {
                        if matches!(request, GdmaTestRequest::Shutdown) {
                            shutdown = true;
                        } else {
                            let request = encode_request(request);
                            inject_eqe(request.eqe_type(), request.data())
                        }
                    })
                    .await;
                    if shutdown {
                        break;
                    }
                }
            })
            .detach();

        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn encode_vf_reset() {
        for revoke_vtl0_vf in [false, true] {
            let request = encode_request(GdmaTestRequest::VfReset { revoke_vtl0_vf });
            assert_eq!(request.eqe_type(), GDMA_EQE_HWC_RESET_REQUEST);
            let EncodedTestRequest::VfReset(data) = request;
            assert_eq!(data.revoke_vtl0_vf(), revoke_vtl0_vf);
        }
    }
}
