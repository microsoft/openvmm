// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM resource-profile resolution.

use super::profile::MICROVM_MOUNT_TAG;
use crate::virtio::VirtioFsDevice;
use virtio_resources::fs::VirtioFsBackend;
use virtio_resources::fs::VirtioFsHandle;
use virtio_resources::fs::microvm::VirtioFsProfile;
use vmcore::vm_task::VmTaskDriverSource;

pub(crate) fn resolve(
    resource: &VirtioFsHandle,
    driver_source: &VmTaskDriverSource,
) -> anyhow::Result<Option<VirtioFsDevice>> {
    let device = match &resource.profile {
        VirtioFsProfile::Standard => return Ok(None),
        VirtioFsProfile::Microvm {
            stable_id,
            root_identity,
            read_only,
        } => {
            anyhow::ensure!(
                resource.tag == MICROVM_MOUNT_TAG,
                "microVM virtio-fs tag must be '{}'",
                MICROVM_MOUNT_TAG
            );
            let VirtioFsBackend::HostFs {
                root_path,
                mount_options,
            } = &resource.fs
            else {
                anyhow::bail!("microVM virtio-fs requires a HostFs backend");
            };
            anyhow::ensure!(
                mount_options.is_empty(),
                "microVM virtio-fs does not accept HostFs mount options"
            );
            VirtioFsDevice::new_microvm_hostfs(
                driver_source,
                stable_id.clone(),
                root_identity.clone(),
                *read_only,
                root_path,
                None,
            )?
        }
    };
    Ok(Some(device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::MICROVM_ATTACHMENT_ID;
    use crate::profile::microvm_root_identity;
    use crate::resolver::VirtioFsResolver;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use virtio::resolve::ResolvedVirtioDevice;
    use virtio::resolve::VirtioResolveInput;
    use virtio_resources::fs::VirtioFsAggregateChild;
    use vm_resource::ResolveResource;
    use vmcore::vm_task::SingleDriverBackend;

    fn resolve(
        driver: DefaultDriver,
        tag: &str,
        fs: VirtioFsBackend,
    ) -> anyhow::Result<ResolvedVirtioDevice> {
        let root_path = match &fs {
            VirtioFsBackend::HostFs { root_path, .. } => Some(root_path),
            _ => None,
        };
        let root_identity = root_path
            .map(microvm_root_identity)
            .transpose()?
            .unwrap_or_else(|| vec![1]);
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        VirtioFsResolver.resolve(
            VirtioFsHandle {
                tag: tag.to_owned(),
                fs,
                profile: VirtioFsProfile::Microvm {
                    stable_id: MICROVM_ATTACHMENT_ID.to_owned(),
                    root_identity,
                    read_only: true,
                },
            },
            VirtioResolveInput {
                driver_source: &driver_source,
            },
        )
    }

    #[async_test]
    async fn microvm_profile_rejects_non_fixed_tag(driver: DefaultDriver) {
        let root = tempfile::tempdir().unwrap();
        let result = resolve(
            driver,
            "other",
            VirtioFsBackend::HostFs {
                root_path: root.path().to_string_lossy().into_owned(),
                mount_options: String::new(),
            },
        );
        assert!(result.is_err());
    }

    #[async_test]
    async fn microvm_profile_rejects_mount_options(driver: DefaultDriver) {
        let root = tempfile::tempdir().unwrap();
        let result = resolve(
            driver,
            MICROVM_MOUNT_TAG,
            VirtioFsBackend::HostFs {
                root_path: root.path().to_string_lossy().into_owned(),
                mount_options: "ro".to_owned(),
            },
        );
        assert!(result.is_err());
    }

    #[async_test]
    async fn microvm_profile_rejects_non_host_backends(driver: DefaultDriver) {
        let result = resolve(
            driver,
            MICROVM_MOUNT_TAG,
            VirtioFsBackend::Aggregate {
                children: vec![VirtioFsAggregateChild {
                    name: "child".to_owned(),
                    root_path: ".".to_owned(),
                    mount_options: String::new(),
                }],
            },
        );
        assert!(result.is_err());
    }

    #[async_test]
    async fn microvm_profile_rejects_section_backend(driver: DefaultDriver) {
        let result = resolve(
            driver,
            MICROVM_MOUNT_TAG,
            VirtioFsBackend::SectionFs {
                root_path: ".".to_owned(),
            },
        );
        assert!(result.is_err());
    }
}
