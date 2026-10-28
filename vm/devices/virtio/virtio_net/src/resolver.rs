// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-net devices.

use crate::Device;
use async_trait::async_trait;
use net_backend::resolve::ResolveEndpointParams;
use net_backend_resources::consomme::ConsommeHandle;
use net_backend_resources::null::NullHandle;
#[cfg(target_os = "linux")]
use net_backend_resources::tap::TapHandle;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-net devices.
pub struct VirtioNetResolver;

fn validate_copy_only_endpoint(id: &str) -> anyhow::Result<()> {
    if id == ConsommeHandle::ID || id == NullHandle::ID {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if id == TapHandle::ID {
        return Ok(());
    }
    anyhow::bail!("copy-only guest memory does not support network endpoint resource '{id}'")
}

declare_static_async_resolver! {
    VirtioNetResolver,
    (VirtioDeviceHandle, VirtioNetHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioNetHandle> for VirtioNetResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioNetHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut builder = Device::builder();
        if input
            .guest_memory
            .is_some_and(|mem| mem.io_policy() == guestmem::GuestMemoryIoPolicy::CopyOnly)
        {
            validate_copy_only_endpoint(resource.endpoint.id())?;
        }
        if let Some(max_queues) = resource.max_queues {
            builder = builder.max_queues(max_queues);
        }

        let endpoint = resolver
            .resolve(
                resource.endpoint,
                ResolveEndpointParams {
                    mac_address: resource.mac_address,
                },
            )
            .await?;

        let device = builder.build(input.driver_source, endpoint.0, resource.mac_address)?;

        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vm_resource::IntoResource;
    use vm_resource::kind::NetEndpointHandleKind;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    #[derive(mesh::MeshPayload)]
    struct UnsupportedEndpoint;

    impl ResourceId<NetEndpointHandleKind> for UnsupportedEndpoint {
        const ID: &'static str = "unsupported-copy-only-test";
    }

    #[async_test]
    async fn copy_only_rejects_endpoint_before_resolution(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let memory = guestmem::GuestMemory::allocate(4096).copy_only_view();
        let resource = VirtioNetHandle {
            max_queues: None,
            mac_address: net_backend_resources::mac_address::MacAddress::new([0; 6]),
            endpoint: UnsupportedEndpoint.into_resource(),
        };
        let error = VirtioNetResolver
            .resolve(
                &ResourceResolver::new(),
                resource,
                VirtioResolveInput {
                    driver_source: &driver_source,
                    guest_memory: Some(&memory),
                },
            )
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("copy-only guest memory does not support network endpoint resource")
        );
    }

    #[test]
    fn copy_only_endpoint_admission() {
        validate_copy_only_endpoint(ConsommeHandle::ID).unwrap();
        validate_copy_only_endpoint(NullHandle::ID).unwrap();
        #[cfg(target_os = "linux")]
        validate_copy_only_endpoint(TapHandle::ID).unwrap();
        for id in ["mana", "dio", "vfio", "vhost", "external", "unknown"] {
            assert!(validate_copy_only_endpoint(id).is_err(), "{id}");
        }
    }
}
