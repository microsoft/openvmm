// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM resource-profile resolution.

use super::profile::MicroVmAggregateChild;
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

fn owner_mode(caller_identity: bool) -> MicroVmOwnerMode {
    if caller_identity {
        MicroVmOwnerMode::Caller
    } else {
        MicroVmOwnerMode::Vmm
    }
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
            allowed_paths,
            writable_paths,
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
                allowed_paths.clone(),
                writable_paths.clone(),
                owner_mode(*caller_identity),
                root_path,
                None,
            )?
        }
        VirtioFsProfile::MicrovmAggregate {
            stable_id,
            children,
            caller_identity,
        } => {
            validate_tag(resource, stable_id)?;
            let VirtioFsBackend::Aggregate { children: roots } = &resource.fs else {
                anyhow::bail!("microVM aggregate virtio-fs requires an Aggregate backend");
            };
            anyhow::ensure!(
                roots.len() == children.len()
                    && roots
                        .iter()
                        .zip(children)
                        .all(|(root, child)| root.name == child.name),
                "microVM aggregate virtio-fs backend children do not match its profile"
            );
            anyhow::ensure!(
                roots.iter().all(|root| root.mount_options.is_empty()),
                "microVM aggregate virtio-fs does not accept HostFs mount options"
            );
            let children = children
                .iter()
                .map(|child| {
                    if child.file {
                        anyhow::ensure!(
                            child.denied_paths.is_empty()
                                && child.allowed_paths.is_empty()
                                && child.writable_paths.is_empty(),
                            "microVM aggregate child '{}' exposes a file, which has no policy paths",
                            child.name
                        );
                        Ok(MicroVmAggregateChild::new_file(
                            child.name.clone(),
                            child.root_identity.clone(),
                            child.read_only,
                        )?)
                    } else {
                        Ok(MicroVmAggregateChild::new(
                            child.name.clone(),
                            child.root_identity.clone(),
                            child.read_only,
                            child.denied_paths.clone(),
                            child.allowed_paths.clone(),
                            child.writable_paths.clone(),
                        )?)
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            VirtioFsDevice::new_microvm_aggregate(
                driver_source,
                stable_id.clone(),
                children,
                owner_mode(*caller_identity),
                &roots
                    .iter()
                    .map(|root| root.root_path.as_str())
                    .collect::<Vec<_>>(),
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
    use crate::profile::microvm_file_identity;
    use crate::profile::microvm_root_identity;
    use crate::resolver::VirtioFsResolver;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::path::Path;
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
                    allowed_paths: Vec::new(),
                    writable_paths: Vec::new(),
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
        resolve_slot(
            driver.clone(),
            MICROVM_ATTACHMENT_ID,
            MICROVM_MOUNT_TAG,
            host_fs(&root),
        )
        .unwrap();
        for (stable_id, tag) in [
            ("fs:microvm1", "microvm1"),
            ("fs:microvm1", MICROVM_MOUNT_TAG),
            (MICROVM_ATTACHMENT_ID, "microvm1"),
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
        dormant(MICROVM_ATTACHMENT_ID, MICROVM_MOUNT_TAG).unwrap();
        assert!(dormant("fs:microvm1", "microvm1").is_err());
    }

    fn resolve_aggregate(
        driver: DefaultDriver,
        profile_names: &[&str],
        fs: VirtioFsBackend,
        roots: &[&tempfile::TempDir],
    ) -> anyhow::Result<ResolvedVirtioDevice> {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        VirtioFsResolver.resolve(
            VirtioFsHandle {
                tag: MICROVM_MOUNT_TAG.to_owned(),
                fs,
                profile: VirtioFsProfile::MicrovmAggregate {
                    stable_id: MICROVM_ATTACHMENT_ID.to_owned(),
                    children: profile_names
                        .iter()
                        .zip(roots)
                        .map(
                            |(name, root)| virtio_resources::fs::microvm::MicrovmAggregateChild {
                                name: (*name).to_owned(),
                                file: false,
                                root_identity: microvm_root_identity(root.path()).unwrap(),
                                read_only: true,
                                denied_paths: Vec::new(),
                                allowed_paths: Vec::new(),
                                writable_paths: Vec::new(),
                            },
                        )
                        .collect(),
                    caller_identity: false,
                },
            },
            VirtioResolveInput {
                driver_source: &driver_source,
            },
        )
    }

    fn aggregate_fs(
        children: &[(&str, &tempfile::TempDir)],
        mount_options: &str,
    ) -> VirtioFsBackend {
        VirtioFsBackend::Aggregate {
            children: children
                .iter()
                .map(|(name, root)| VirtioFsAggregateChild {
                    name: (*name).to_owned(),
                    root_path: root.path().to_string_lossy().into_owned(),
                    mount_options: mount_options.to_owned(),
                })
                .collect(),
        }
    }

    #[async_test]
    async fn microvm_aggregate_profile_requires_matching_children(driver: DefaultDriver) {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let roots = [&first, &second];
        resolve_aggregate(
            driver.clone(),
            &["a", "b"],
            aggregate_fs(&[("a", &first), ("b", &second)], ""),
            &roots,
        )
        .unwrap();
        for fs in [
            aggregate_fs(&[("b", &second), ("a", &first)], ""),
            aggregate_fs(&[("a", &first)], ""),
            aggregate_fs(&[("a", &first), ("b", &second)], "ro"),
            host_fs(&first),
        ] {
            assert!(resolve_aggregate(driver.clone(), &["a", "b"], fs, &roots).is_err());
        }
        // Each child root must match its identity.
        assert!(
            resolve_aggregate(
                driver,
                &["a", "b"],
                aggregate_fs(&[("a", &second), ("b", &first)], ""),
                &roots,
            )
            .is_err()
        );
    }

    #[async_test]
    async fn microvm_aggregate_profile_resolves_file_children(driver: DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("settings.json");
        std::fs::write(&file, b"{}").unwrap();
        let resolve_file = |identity: Vec<u8>, root_path: &Path, denied_paths: Vec<String>| {
            let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
            VirtioFsResolver.resolve(
                VirtioFsHandle {
                    tag: MICROVM_MOUNT_TAG.to_owned(),
                    fs: VirtioFsBackend::Aggregate {
                        children: vec![VirtioFsAggregateChild {
                            name: "0".to_owned(),
                            root_path: root_path.to_string_lossy().into_owned(),
                            mount_options: String::new(),
                        }],
                    },
                    profile: VirtioFsProfile::MicrovmAggregate {
                        stable_id: MICROVM_ATTACHMENT_ID.to_owned(),
                        children: vec![virtio_resources::fs::microvm::MicrovmAggregateChild {
                            name: "0".to_owned(),
                            file: true,
                            root_identity: identity,
                            read_only: true,
                            denied_paths,
                            allowed_paths: Vec::new(),
                            writable_paths: Vec::new(),
                        }],
                        caller_identity: false,
                    },
                },
                VirtioResolveInput {
                    driver_source: &driver_source,
                },
            )
        };
        let identity = microvm_file_identity(&file).unwrap();
        resolve_file(identity.clone(), &file, Vec::new()).unwrap();
        // A file has no policy paths.
        assert!(resolve_file(identity.clone(), &file, vec!["x".to_owned()]).is_err());
        // The root must be the file that the identity names, and a regular
        // file, not its directory.
        assert!(resolve_file(identity, directory.path(), Vec::new()).is_err());
        assert!(
            resolve_file(
                microvm_root_identity(directory.path()).unwrap(),
                directory.path(),
                Vec::new()
            )
            .is_err()
        );
        assert!(microvm_file_identity(directory.path()).is_err());
        assert!(microvm_root_identity(&file).is_err());
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
