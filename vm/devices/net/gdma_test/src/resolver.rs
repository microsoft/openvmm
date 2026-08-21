// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for [`GdmaTestDeviceHandle`].

use async_trait::async_trait;
use futures::StreamExt;
use gdma::GdmaDevice;
use gdma::resolver::Error;
use gdma::test_helpers::hwc_eq_injector;
use gdma::test_helpers::resolve_vports;
use gdma_defs::EqeDataReconfig;
use gdma_defs::EqeVfReset;
use gdma_defs::GDMA_EQE_HWC_RECONFIG_DATA;
use gdma_defs::GDMA_EQE_HWC_RESET_REQUEST;
use gdma_defs::HWC_DATA_TYPE_HW_VPORT_LINK_CONNECT;
use gdma_defs::HWC_DATA_TYPE_HW_VPORT_LINK_DISCONNECT;
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
/// Creates a standard GDMA device and spawns a detached background task that
/// translates test requests into EQEs injected directly into the HWC EQ via
/// [`gdma::test_helpers::hwc_eq_injector`].
pub struct GdmaTestDeviceResolver;

declare_static_async_resolver! {
    GdmaTestDeviceResolver,
    (PciDeviceHandleKind, GdmaTestDeviceHandle),
}

enum EncodedTestRequest {
    VfReset(EqeVfReset),
    VportLinkState(EqeDataReconfig),
}

impl EncodedTestRequest {
    fn eqe_type(&self) -> u8 {
        match self {
            Self::VfReset(_) => GDMA_EQE_HWC_RESET_REQUEST,
            Self::VportLinkState(_) => GDMA_EQE_HWC_RECONFIG_DATA,
        }
    }

    fn data(&self) -> &[u8] {
        match self {
            Self::VfReset(data) => data.as_bytes(),
            Self::VportLinkState(data) => data.as_bytes(),
        }
    }
}

fn encode_request(request: GdmaTestRequest) -> EncodedTestRequest {
    match request {
        GdmaTestRequest::VfReset { revoke_vtl0_vf } => {
            EncodedTestRequest::VfReset(EqeVfReset::new().with_revoke_vtl0_vf(revoke_vtl0_vf))
        }
        GdmaTestRequest::VportLinkState { vport, connected } => {
            let data_type = if connected {
                HWC_DATA_TYPE_HW_VPORT_LINK_CONNECT
            } else {
                HWC_DATA_TYPE_HW_VPORT_LINK_DISCONNECT
            };
            let vport = vport.to_le_bytes();
            EncodedTestRequest::VportLinkState(EqeDataReconfig {
                data: [vport[0], vport[1], vport[2]],
                data_type,
                reserved1: [0; 8],
            })
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
                    rpc.handle(async |request| {
                        let request = encode_request(request);
                        inject_eqe(request.eqe_type(), request.data())
                    })
                    .await;
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
            let EncodedTestRequest::VfReset(data) = request else {
                panic!("VF reset encoded as the wrong event type");
            };
            assert_eq!(data.revoke_vtl0_vf(), revoke_vtl0_vf);
        }
    }

    #[test]
    fn encode_vport_link_state() {
        for (connected, expected_data_type) in [
            (false, HWC_DATA_TYPE_HW_VPORT_LINK_DISCONNECT),
            (true, HWC_DATA_TYPE_HW_VPORT_LINK_CONNECT),
        ] {
            let request = encode_request(GdmaTestRequest::VportLinkState {
                vport: 0x00ab_cdef,
                connected,
            });
            assert_eq!(request.eqe_type(), GDMA_EQE_HWC_RECONFIG_DATA);
            let EncodedTestRequest::VportLinkState(data) = request else {
                panic!("vport link state encoded as the wrong event type");
            };
            assert_eq!(data.data, [0xef, 0xcd, 0xab]);
            assert_eq!(data.data_type, expected_data_type);
            assert_eq!(data.reserved1, [0; 8]);
        }
    }
}
