// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtiofs devices.

use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use anyhow::Context as _;
use lxutil::LxVolumeOptions;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::fs::VirtioFsAggregateChild;
use virtio_resources::fs::VirtioFsBackend;
use virtio_resources::fs::VirtioFsHandle;
use virtio_resources::fs::VirtioFsWindowsOptions;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

const VIRTIO_FS_TAG_LEN: usize = 36;
#[cfg(windows)]
const SECTION_FS_SHARED_MEMORY_SIZE: u64 = 8 * 1024 * 1024 * 1024;

struct ParsedVirtioFs {
    tag: String,
    backend: ParsedVirtioFsBackend,
}

enum ParsedVirtioFsBackend {
    HostFs {
        root_path: String,
        mount_options: LxVolumeOptions,
    },
    #[cfg(windows)]
    SectionFs { root_path: String },
    Aggregate {
        children: Vec<VirtioFsAggregateChild>,
    },
}

impl ParsedVirtioFs {
    fn parse(resource: VirtioFsHandle) -> anyhow::Result<Self> {
        let VirtioFsHandle { tag, fs } = resource;
        anyhow::ensure!(!tag.is_empty(), "virtio-fs tag must not be empty");
        anyhow::ensure!(
            !tag.contains('\0'),
            "virtio-fs tag must not contain NUL bytes"
        );
        anyhow::ensure!(
            tag.len() <= VIRTIO_FS_TAG_LEN,
            "virtio-fs tag exceeds the {VIRTIO_FS_TAG_LEN}-byte protocol limit"
        );

        let backend = match fs {
            VirtioFsBackend::HostFs {
                root_path,
                mount_options,
                read_only,
                windows_options,
            } => {
                anyhow::ensure!(
                    !root_path.is_empty(),
                    "virtio-fs root path must not be empty"
                );
                anyhow::ensure!(
                    !root_path.contains('\0'),
                    "virtio-fs root path must not contain NUL bytes"
                );

                return parse_host_fs(root_path, mount_options, read_only, windows_options)
                    .map(|backend| Self { tag, backend });
            }
            #[cfg(windows)]
            VirtioFsBackend::SectionFs { root_path } => {
                ParsedVirtioFsBackend::SectionFs { root_path }
            }
            #[cfg(not(windows))]
            VirtioFsBackend::SectionFs { .. } => {
                anyhow::bail!("section fs not supported on this platform")
            }
            VirtioFsBackend::Aggregate { children } => {
                ParsedVirtioFsBackend::Aggregate { children }
            }
        };

        Ok(Self { tag, backend })
    }
}

fn parse_host_fs(
    root_path: String,
    mut mount_options: String,
    read_only: bool,
    windows_options: Option<VirtioFsWindowsOptions>,
) -> anyhow::Result<ParsedVirtioFsBackend> {
    if windows_options.is_some_and(|options| options.shared_mem) {
        #[cfg(windows)]
        {
            anyhow::ensure!(
                !read_only && mount_options.is_empty(),
                "virtio-fs shared_mem does not support read_only or mount_options"
            );
            return Ok(ParsedVirtioFsBackend::SectionFs { root_path });
        }
        #[cfg(not(windows))]
        anyhow::bail!("virtio-fs windows_options are only supported on Windows");
    }
    if read_only {
        if !mount_options.is_empty() {
            mount_options.push(';');
        }
        mount_options.push_str("ro");
    }
    Ok(ParsedVirtioFsBackend::HostFs {
        root_path,
        mount_options: LxVolumeOptions::from_option_string(&mount_options),
    })
}

/// Resolver for virtiofs devices.
pub struct VirtioFsResolver;

declare_static_resolver! {
    VirtioFsResolver,
    (VirtioDeviceHandle, VirtioFsHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioFsHandle> for VirtioFsResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioFsHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let ParsedVirtioFs { tag, backend } = ParsedVirtioFs::parse(resource)?;
        let device = match backend {
            ParsedVirtioFsBackend::HostFs {
                root_path,
                mount_options,
            } => VirtioFsDevice::new(
                input.driver_source,
                &tag,
                VirtioFs::new(&root_path, Some(&mount_options))?,
                0,
                None,
            ),
            #[cfg(windows)]
            ParsedVirtioFsBackend::SectionFs { root_path } => VirtioFsDevice::new(
                input.driver_source,
                &tag,
                crate::SectionFs::new(&root_path)?,
                SECTION_FS_SHARED_MEMORY_SIZE,
                None,
            ),
            ParsedVirtioFsBackend::Aggregate { children } => {
                let fs = VirtioFs::new_aggregate();
                for child in children {
                    fs.add_child(
                        &child.name,
                        &child.root_path,
                        Some(&LxVolumeOptions::from_option_string(&child.mount_options)),
                    )
                    .with_context(|| {
                        format!(
                            "failed to add virtiofs aggregate child '{}' (root_path='{}')",
                            child.name, child.root_path
                        )
                    })?;
                }
                VirtioFsDevice::new(input.driver_source, &tag, fs, 0, None)
            }
        };
        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedVirtioFs;
    use super::ParsedVirtioFsBackend;
    use test_with_tracing::test;
    use virtio_resources::fs::VirtioFsBackend;
    use virtio_resources::fs::VirtioFsHandle;
    use virtio_resources::fs::VirtioFsWindowsOptions;

    fn host_fs(tag: &str, root_path: &str, read_only: bool, mount_options: &str) -> VirtioFsHandle {
        VirtioFsHandle {
            tag: tag.to_owned(),
            fs: VirtioFsBackend::HostFs {
                root_path: root_path.to_owned(),
                mount_options: mount_options.to_owned(),
                read_only,
                windows_options: None,
            },
        }
    }

    #[test]
    fn parse_rejects_invalid_tag_and_root_path() {
        assert!(ParsedVirtioFs::parse(host_fs("", "root", false, "")).is_err());
        assert!(ParsedVirtioFs::parse(host_fs("tag", "", false, "")).is_err());
        assert!(ParsedVirtioFs::parse(host_fs("tag", "root\0path", false, "")).is_err());
    }

    #[test]
    fn parse_applies_read_only_mount_option() {
        let parsed = ParsedVirtioFs::parse(host_fs("tag", "root", true, "uid=1000"))
            .expect("parse virtio-fs resource");
        let ParsedVirtioFsBackend::HostFs { mount_options, .. } = parsed.backend else {
            panic!("expected host filesystem backend");
        };
        assert!(mount_options.is_readonly());
    }

    #[test]
    fn parse_shared_memory_options() {
        let mut resource = host_fs("tag", "root", false, "");
        let VirtioFsBackend::HostFs {
            windows_options, ..
        } = &mut resource.fs
        else {
            unreachable!();
        };
        *windows_options = Some(VirtioFsWindowsOptions { shared_mem: true });
        let result = ParsedVirtioFs::parse(resource);
        #[cfg(windows)]
        assert!(matches!(
            result.unwrap().backend,
            ParsedVirtioFsBackend::SectionFs { .. }
        ));
        #[cfg(not(windows))]
        assert!(result.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn parse_rejects_host_options_for_shared_memory() {
        for (read_only, mount_options) in [(true, ""), (false, "uid=1000")] {
            let mut resource = host_fs("tag", "root", read_only, mount_options);
            let VirtioFsBackend::HostFs {
                windows_options, ..
            } = &mut resource.fs
            else {
                unreachable!();
            };
            *windows_options = Some(VirtioFsWindowsOptions { shared_mem: true });
            assert!(ParsedVirtioFs::parse(resource).is_err());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_accepts_empty_windows_options_on_non_windows() {
        let mut resource = host_fs("tag", "root", false, "");
        let VirtioFsBackend::HostFs {
            windows_options, ..
        } = &mut resource.fs
        else {
            unreachable!();
        };
        *windows_options = Some(Default::default());
        assert!(ParsedVirtioFs::parse(resource).is_ok());
    }
}
