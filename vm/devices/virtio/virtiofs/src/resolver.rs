// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtiofs devices.

use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
#[cfg(windows)]
use crate::virtio::VirtioFsDeviceOptions;
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
use vmcore::vm_task::VmTaskDriverSource;

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
                windows_options,
            } => {
                validate_root_path(&root_path)?;
                if windows_options
                    .as_ref()
                    .is_some_and(|options| options.shared_mem)
                {
                    parse_shared_mem(root_path, &mount_options, windows_options)?
                } else {
                    let platform = ParsedHostFsPlatform::parse(windows_options)?;
                    ParsedVirtioFsBackend::HostFs {
                        root_path,
                        mount_options: LxVolumeOptions::from_option_string(&mount_options),
                        platform,
                    }
                }
            }
            #[cfg(windows)]
            VirtioFsBackend::SectionFs { root_path } => {
                validate_root_path(&root_path)?;
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

/// Parses a host filesystem share that requests shared-memory section
/// objects instead of host files.
fn parse_shared_mem(
    root_path: String,
    mount_options: &str,
    windows_options: Option<VirtioFsWindowsOptions>,
) -> anyhow::Result<ParsedVirtioFsBackend> {
    cfg_select! {
        windows => {
            anyhow::ensure!(
                mount_options.is_empty(),
                "virtio-fs shared_mem does not support mount_options"
            );
            anyhow::ensure!(
                windows_options
                    .and_then(|options| options.user_token)
                    .is_none(),
                "virtio-fs shared_mem does not support user tokens"
            );
            Ok(ParsedVirtioFsBackend::SectionFs { root_path })
        }
        _ => {
            let _ = (root_path, mount_options, windows_options);
            anyhow::bail!("virtio-fs shared_mem is only supported on Windows")
        }
    }
}

fn validate_root_path(root_path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !root_path.is_empty(),
        "virtio-fs root path must not be empty"
    );
    anyhow::ensure!(
        !root_path.contains('\0'),
        "virtio-fs root path must not contain NUL bytes"
    );
    Ok(())
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
            } => {
                let fs = platform.open_fs(&root_path, &mount_options)?;
                let driver_source = platform.driver_source(input.driver_source, &tag)?;
                VirtioFsDevice::new(&driver_source, &tag, fs, Default::default())
            }
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
                    .with_context(|| {
                        format!(
                            "failed to add virtiofs aggregate child '{}' (root_path='{}')",
                            child.name, child.root_path
                        )
                    })?;
                }
                VirtioFsDevice::new(input.driver_source, &tag, fs, Default::default())
            }
        };
        Ok(device.into())
    }
}

cfg_select! {
    windows => {
        use pal::windows::security::RevertToSelfGuard;
        use pal::windows::security::duplicate_impersonation_token;
        use pal_async::DefaultPool;
        use std::os::windows::io::AsHandle;
        use std::os::windows::io::OwnedHandle;
        use vmcore::vm_task::SingleDriverBackend;

        struct ParsedHostFsPlatform {
            user_token: Option<OwnedHandle>,
        }

        impl ParsedHostFsPlatform {
            fn parse(windows_options: Option<VirtioFsWindowsOptions>) -> anyhow::Result<Self> {
                let user_token = windows_options
                    .and_then(|options| options.user_token)
                    .map(|token| duplicate_impersonation_token(token.as_handle()))
                    .transpose()
                    .context("invalid virtio-fs user token")?;

                Ok(Self { user_token })
            }

            fn open_fs(
                &self,
                root_path: &str,
                mount_options: &LxVolumeOptions,
            ) -> anyhow::Result<VirtioFs> {
                let _impersonation = self
                    .user_token
                    .as_ref()
                    .map(RevertToSelfGuard::impersonate)
                    .transpose()
                    .context("failed to impersonate virtio-fs user token")?;
                Ok(VirtioFs::new(root_path, Some(mount_options))?)
            }

            fn driver_source(
                self,
                driver_source: &VmTaskDriverSource,
                tag: &str,
            ) -> anyhow::Result<VmTaskDriverSource> {
                match self.user_token {
                    Some(token) => impersonating_driver_source(format!("virtiofs-{tag}"), token),
                    None => Ok(driver_source.clone()),
                }
            }
        }

        /// Returns a driver source whose tasks and IO run on a new IOCP thread
        /// that impersonates `token` for the thread's lifetime.
        ///
        /// This avoids setting and reverting the thread token for every FUSE
        /// request. The thread exits once the last driver is dropped.
        pub(crate) fn impersonating_driver_source(
            name: String,
            token: OwnedHandle,
        ) -> anyhow::Result<VmTaskDriverSource> {
            let pool = DefaultPool::new();
            let driver = pool.driver();
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    let _impersonation = match RevertToSelfGuard::impersonate(&token) {
                        Ok(guard) => guard,
                        Err(err) => {
                            let _ = send.send(Err(err));
                            return;
                        }
                    };
                    let _ = send.send(Ok(()));
                    pool.run();
                })
                .context("failed to spawn virtio-fs impersonation thread")?;
            recv.recv()
                .context("virtio-fs impersonation thread exited")?
                .context("failed to impersonate virtio-fs user token")?;
            Ok(VmTaskDriverSource::new(SingleDriverBackend::new(driver)))
        }
    }
    _ => {
        struct ParsedHostFsPlatform;

        impl ParsedHostFsPlatform {
            fn parse(windows_options: Option<VirtioFsWindowsOptions>) -> anyhow::Result<Self> {
                // The options carry no fields on non-Windows hosts.
                let _ = windows_options;
                Ok(Self)
            }

            fn open_fs(
                &self,
                root_path: &str,
                mount_options: &LxVolumeOptions,
            ) -> anyhow::Result<VirtioFs> {
                Ok(VirtioFs::new(root_path, Some(mount_options))?)
            }

            fn driver_source(
                self,
                driver_source: &VmTaskDriverSource,
                _tag: &str,
            ) -> anyhow::Result<VmTaskDriverSource> {
                Ok(driver_source.clone())
            }
        }
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

    fn host_fs(tag: &str, root_path: &str, mount_options: &str) -> VirtioFsHandle {
        VirtioFsHandle {
            tag: tag.to_owned(),
            fs: VirtioFsBackend::HostFs {
                root_path: root_path.to_owned(),
                mount_options: mount_options.to_owned(),
                windows_options: None,
            },
        }
    }

    fn shared_mem_fs(mount_options: &str) -> VirtioFsHandle {
        let mut resource = host_fs("tag", "root", mount_options);
        let VirtioFsBackend::HostFs {
            windows_options, ..
        } = &mut resource.fs
        else {
            unreachable!();
        };
        *windows_options = Some(VirtioFsWindowsOptions {
            #[cfg(windows)]
            user_token: None,
            shared_mem: true,
        });
        resource
    }

    #[test]
    fn parse_rejects_invalid_tag_and_root_path() {
        assert!(ParsedVirtioFs::parse(host_fs("", "root", "")).is_err());
        assert!(ParsedVirtioFs::parse(host_fs("tag", "", "")).is_err());
        assert!(ParsedVirtioFs::parse(host_fs("tag", "root\0path", "")).is_err());
    }

    #[test]
    fn parse_applies_read_only_mount_option() {
        for (options, readonly) in [
            ("", false),
            ("uid=1000", false),
            ("ro", true),
            ("uid=1000;ro", true),
        ] {
            let parsed = ParsedVirtioFs::parse(host_fs("tag", "root", options))
                .expect("parse virtio-fs resource");
            let ParsedVirtioFsBackend::HostFs { mount_options, .. } = parsed.backend else {
                panic!("expected host filesystem backend");
            };
            assert_eq!(mount_options.is_readonly(), readonly);
        }
    }

    #[test]
    fn parse_shared_memory_options() {
        let result = ParsedVirtioFs::parse(shared_mem_fs(""));
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
        for mount_options in ["ro", "uid=1000"] {
            assert!(ParsedVirtioFs::parse(shared_mem_fs(mount_options)).is_err());
        }

        let mut resource = shared_mem_fs("");
        let VirtioFsBackend::HostFs {
            windows_options: Some(options),
            ..
        } = &mut resource.fs
        else {
            unreachable!();
        };
        options.user_token = Some(crate::integration_tests::user_token::restricted_token());
        assert!(ParsedVirtioFs::parse(resource).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_accepts_empty_windows_options_on_non_windows() {
        let mut resource = host_fs("tag", "root", "");
        let VirtioFsBackend::HostFs {
            windows_options, ..
        } = &mut resource.fs
        else {
            unreachable!();
        };
        *windows_options = Some(Default::default());
        assert!(ParsedVirtioFs::parse(resource).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_rejects_windows_backends_on_non_windows() {
        let resource = VirtioFsHandle {
            tag: "tag".to_owned(),
            fs: VirtioFsBackend::SectionFs {
                root_path: "root".to_owned(),
            },
        };
        let error = ParsedVirtioFs::parse(resource).err().unwrap();
        assert_eq!(
            error.to_string(),
            "section fs not supported on this platform"
        );
    }

    #[cfg(windows)]
    #[test]
    fn open_fs_uses_user_token() {
        use crate::integration_tests::user_token::restricted_token;
        use crate::integration_tests::user_token::thread_token;
        use pal::windows::security::LocalSecurityDescriptor;
        use pal::windows::security::create_directory_with_security;
        use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;

        fn assert_no_thread_token() {
            assert_eq!(
                thread_token().unwrap_err().raw_os_error(),
                Some(ERROR_NO_TOKEN as i32)
            );
        }

        fn open(root: &std::path::Path, user_token: bool) -> anyhow::Result<()> {
            let mut resource = host_fs("tag", root.to_str().unwrap(), "");
            let VirtioFsBackend::HostFs {
                windows_options, ..
            } = &mut resource.fs
            else {
                unreachable!();
            };
            *windows_options = Some(VirtioFsWindowsOptions {
                user_token: user_token.then(restricted_token),
                shared_mem: false,
            });
            let parsed = ParsedVirtioFs::parse(resource)?;
            let ParsedVirtioFsBackend::HostFs {
                root_path,
                mount_options,
                platform,
            } = parsed.backend
            else {
                panic!("expected host filesystem backend");
            };
            platform.open_fs(&root_path, &mount_options)?;
            Ok(())
        }

        // The restricted token passes an access check only if it is granted
        // to Everyone, so it can open `public` but not `private`, which this
        // process can open.
        let dir = tempfile::tempdir().unwrap();
        let public = dir.path().join("public");
        let private = dir.path().join("private");
        for (path, sddl) in [
            (&public, "D:P(A;OICI;FA;;;WD)"),
            (&private, "D:P(A;OICI;FA;;;AU)"),
        ] {
            let sd: LocalSecurityDescriptor = sddl.parse().unwrap();
            create_directory_with_security(path, &sd).unwrap();
        }

        open(&private, false).unwrap();
        assert_no_thread_token();
        open(&public, true).unwrap();
        assert_no_thread_token();
        open(&private, true).unwrap_err();
        assert_no_thread_token();
    }
}
