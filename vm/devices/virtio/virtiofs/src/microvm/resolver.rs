// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM resource-profile resolution.

use super::profile::MicroVmOwnerMode;
use super::profile::microvm_mount_tag;
use crate::virtio::VirtioFsDevice;
use virtio_resources::fs::VirtioFsBackend;
use virtio_resources::fs::VirtioFsHandle;
use virtio_resources::fs::microvm::VirtioFsProfile;
use vmcore::vm_task::VmTaskDriverSource;

/// Rejects a resource whose tag is not the fixed tag of its slot.
fn validate_tag(resource: &VirtioFsHandle, stable_id: &str) -> anyhow::Result<()> {
    let tag = microvm_mount_tag(stable_id).ok_or_else(|| {
        anyhow::anyhow!("microVM virtio-fs attachment ID '{stable_id}' is not a fixed slot")
    })?;
    anyhow::ensure!(
        resource.tag == tag,
        "microVM virtio-fs tag for '{stable_id}' must be '{tag}'"
    );
    Ok(())
}

pub(crate) fn resolve(
    resource: &VirtioFsHandle,
    driver_source: &VmTaskDriverSource,
) -> anyhow::Result<Option<VirtioFsDevice>> {
    let device = match &resource.profile {
        VirtioFsProfile::Standard => return Ok(None),
        VirtioFsProfile::MicrovmDormant { stable_id } => {
            validate_tag(resource, stable_id)?;
            anyhow::ensure!(
                matches!(resource.fs, VirtioFsBackend::Dormant),
                "dormant microVM virtio-fs cannot have an active backend"
            );
            VirtioFsDevice::new_microvm_dormant(driver_source, stable_id.clone(), None)?
        }
        VirtioFsProfile::Microvm {
            stable_id,
            root_identity,
            read_only,
            denied_paths,
            caller_identity,
        } => {
            validate_tag(resource, stable_id)?;
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
                denied_paths.clone(),
                if *caller_identity {
                    MicroVmOwnerMode::Caller
                } else {
                    MicroVmOwnerMode::Vmm
                },
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
    use crate::profile::MICROVM_MOUNT_TAG;
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
        resolve_slot(driver, MICROVM_ATTACHMENT_ID, tag, fs)
    }

    fn resolve_slot(
        driver: DefaultDriver,
        stable_id: &str,
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
                    stable_id: stable_id.to_owned(),
                    root_identity,
                    read_only: true,
                    denied_paths: Vec::new(),
                    caller_identity: false,
                },
            },
            VirtioResolveInput {
                driver_source: &driver_source,
            },
        )
    }

    fn host_fs(root: &tempfile::TempDir) -> VirtioFsBackend {
        VirtioFsBackend::HostFs {
            root_path: root.path().to_string_lossy().into_owned(),
            mount_options: String::new(),
        }
    }

    #[async_test]
    async fn microvm_profile_requires_the_tag_of_its_slot(driver: DefaultDriver) {
        let root = tempfile::tempdir().unwrap();
        resolve_slot(driver.clone(), "fs:microvm1", "microvm1", host_fs(&root)).unwrap();
        for (stable_id, tag) in [
            ("fs:microvm1", MICROVM_MOUNT_TAG),
            (MICROVM_ATTACHMENT_ID, "microvm1"),
            ("fs:microvm2", "microvm2"),
        ] {
            assert!(resolve_slot(driver.clone(), stable_id, tag, host_fs(&root)).is_err());
        }

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let dormant = |stable_id: &str, tag: &str| {
            VirtioFsResolver.resolve(
                VirtioFsHandle {
                    tag: tag.to_owned(),
                    fs: VirtioFsBackend::Dormant,
                    profile: VirtioFsProfile::MicrovmDormant {
                        stable_id: stable_id.to_owned(),
                    },
                },
                VirtioResolveInput {
                    driver_source: &driver_source,
                },
            )
        };
        dormant("fs:microvm1", "microvm1").unwrap();
        assert!(dormant("fs:microvm1", MICROVM_MOUNT_TAG).is_err());
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
