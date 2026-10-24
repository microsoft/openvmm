// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtiofs devices.

use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use crate::virtio::VirtioFsDeviceOptions;
use cfg_if::cfg_if;
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

cfg_if! {
    if #[cfg(windows)] {
        use crate::impersonation::duplicate_user_token;
        use anyhow::Context as _;
        use std::os::windows::io::OwnedHandle;
    }
}

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
        platform: ParsedHostFsPlatform,
    },
    #[cfg(windows)]
    SectionFs { root_path: String },
    Aggregate {
        children: Vec<VirtioFsAggregateChild>,
    },
}

cfg_if! {
    if #[cfg(windows)] {
        struct ParsedHostFsPlatform {
            user_token: Option<OwnedHandle>,
        }

        impl ParsedHostFsPlatform {
            fn open_fs(
                &self,
                root_path: &str,
                mount_options: &LxVolumeOptions,
            ) -> anyhow::Result<VirtioFs> {
                let _impersonation = self
                    .user_token
                    .as_ref()
                    .map(crate::impersonation::ImpersonationGuard::new)
                    .transpose()
                    .context("failed to impersonate virtio-fs user token")?;
                Ok(VirtioFs::new(root_path, Some(mount_options))?)
            }

            fn device_options(self) -> VirtioFsDeviceOptions {
                VirtioFsDeviceOptions {
                    user_token: self.user_token,
                    ..Default::default()
                }
            }
        }
    } else {
        struct ParsedHostFsPlatform;

        impl ParsedHostFsPlatform {
            fn open_fs(
                &self,
                root_path: &str,
                mount_options: &LxVolumeOptions,
            ) -> anyhow::Result<VirtioFs> {
                Ok(VirtioFs::new(root_path, Some(mount_options))?)
            }

            fn device_options(self) -> VirtioFsDeviceOptions {
                Default::default()
            }
        }
    }
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

cfg_if! {
    if #[cfg(windows)] {
        fn parse_host_fs(
            root_path: String,
            mut mount_options: String,
            read_only: bool,
            windows_options: Option<VirtioFsWindowsOptions>,
        ) -> anyhow::Result<ParsedVirtioFsBackend> {
            if let Some(options) = windows_options.as_ref().filter(|options| options.shared_mem) {
                anyhow::ensure!(
                    !read_only && mount_options.is_empty() && options.user_token_handle == 0,
                    "virtio-fs shared_mem does not support read_only, mount_options, or user_token_handle"
                );
                return Ok(ParsedVirtioFsBackend::SectionFs { root_path });
            }

            let user_token = windows_options
                .as_ref()
                .map(|options| options.user_token_handle)
                .filter(|handle| *handle != 0)
                .map(duplicate_user_token)
                .transpose()
                .context("invalid virtio-fs user token")?;

            if read_only {
                if !mount_options.is_empty() {
                    mount_options.push(';');
                }
                mount_options.push_str("ro");
            }
            Ok(ParsedVirtioFsBackend::HostFs {
                root_path,
                mount_options: LxVolumeOptions::from_option_string(&mount_options),
                platform: ParsedHostFsPlatform { user_token },
            })
        }
    } else {
        fn parse_host_fs(
            root_path: String,
            mut mount_options: String,
            read_only: bool,
            windows_options: Option<VirtioFsWindowsOptions>,
        ) -> anyhow::Result<ParsedVirtioFsBackend> {
            if let Some(options) = windows_options {
                anyhow::ensure!(
                    options.user_token_handle == 0 && !options.shared_mem,
                    "virtio-fs windows_options are only supported on Windows"
                );
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
                platform: ParsedHostFsPlatform,
            })
        }
    }
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
                platform,
            } => VirtioFsDevice::new(
                input.driver_source,
                &tag,
                platform.open_fs(&root_path, &mount_options)?,
                platform.device_options(),
            ),
            #[cfg(windows)]
            ParsedVirtioFsBackend::SectionFs { root_path } => VirtioFsDevice::new(
                input.driver_source,
                &tag,
                crate::SectionFs::new(&root_path)?,
                VirtioFsDeviceOptions {
                    shared_memory_size: SECTION_FS_SHARED_MEMORY_SIZE,
                    ..Default::default()
                },
            ),
            ParsedVirtioFsBackend::Aggregate { children } => {
                let fs = VirtioFs::new_aggregate();
                for child in children {
                    fs.add_child(
                        &child.name,
                        &child.root_path,
                        Some(&LxVolumeOptions::from_option_string(&child.mount_options)),
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "failed to add virtiofs aggregate child '{}' (root_path='{}'): {e}",
                            child.name,
                            child.root_path
                        )
                    })?;
                }
                VirtioFsDevice::new(input.driver_source, &tag, fs, Default::default())
            }
        };
        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedVirtioFs;
    use super::ParsedVirtioFsBackend;
    use virtio_resources::fs::VirtioFsBackend;
    use virtio_resources::fs::VirtioFsHandle;

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
