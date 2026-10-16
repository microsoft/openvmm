// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-blk devices.

use crate::VirtioBlkDevice;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::blk::VirtioBlkBounceDiskHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-blk devices.
pub struct VirtioBlkResolver;

fn validate_bounce_resource_id(id: &str) -> anyhow::Result<()> {
    if id != VirtioBlkBounceDiskHandle::ID {
        anyhow::bail!("bounce I/O requires a reviewed file, RAM, or Linux block-device resource");
    }
    Ok(())
}

fn validate_bounce_file(file: &std::fs::File) -> anyhow::Result<()> {
    if !file.metadata()?.is_file() {
        anyhow::bail!("bounce I/O requires a regular file, not a host device");
    }
    validate_bounce_flags(file, false)
}

#[cfg(target_os = "linux")]
fn validate_bounce_block_device(file: &std::fs::File) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    anyhow::ensure!(
        file.metadata()?.file_type().is_block_device(),
        "bounce I/O requires a Linux block device"
    );
    validate_bounce_flags(file, true)
}

fn validate_bounce_flags(file: &std::fs::File, allow_direct: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::FcntlArg;
        use nix::fcntl::OFlag;
        use nix::fcntl::fcntl;
        let flags = OFlag::from_bits_retain(fcntl(file, FcntlArg::F_GETFL)?);
        if !allow_direct && flags.contains(OFlag::O_DIRECT) {
            anyhow::bail!("bounce I/O requires a buffered file, not O_DIRECT");
        }
        if flags.contains(OFlag::O_APPEND) {
            anyhow::bail!("bounce I/O does not support O_APPEND file descriptors");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, allow_direct);
    Ok(())
}

declare_static_async_resolver! {
    VirtioBlkResolver,
    (VirtioDeviceHandle, VirtioBlkHandle),
    (DiskHandleKind, VirtioBlkBounceDiskHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioBlkHandle> for VirtioBlkResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        mut resource: VirtioBlkHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let bounce_io = input.bounce_io(resource.bounce_io);
        if bounce_io && let Some(disk) = resource.bounce_disk {
            resource.disk = disk;
        }
        let bounce_supported = resource.disk.id() == VirtioBlkBounceDiskHandle::ID;
        let deferred_bounce = bounce_supported && input.guest_memory.is_none();
        if bounce_io {
            validate_bounce_resource_id(resource.disk.id())?;
        }
        let disk = resolver
            .resolve(
                resource.disk,
                ResolveDiskParameters {
                    read_only: resource.read_only,
                    driver_source: input.driver_source,
                },
            )
            .await?;
        if (bounce_io || deferred_bounce)
            && disk.0.sector_size() as usize > scsi_buffers::MAX_BOUNCE_IO_SIZE
        {
            anyhow::bail!("bounce I/O does not support sectors larger than the request limit");
        }

        let device = VirtioBlkDevice::new(
            input.driver_source,
            disk.0,
            resource.read_only,
            resource.serial,
        )?;
        let device = if deferred_bounce {
            device.with_bounce_support()
        } else {
            device
        };
        let device = if bounce_io {
            device.with_bounce_io()
        } else {
            device
        };
        Ok(device.into())
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, VirtioBlkBounceDiskHandle> for VirtioBlkResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioBlkBounceDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = match resource {
            VirtioBlkBounceDiskHandle::File(file) => {
                validate_bounce_file(&file)?;
                disk_backend_resources::FileDiskHandle(file).into_resource()
            }
            VirtioBlkBounceDiskHandle::Ram { len } => {
                if len == 0 || len % 512 != 0 {
                    anyhow::bail!("bounce RAM disk length must be a positive multiple of 512");
                }
                LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(len),
                    sector_size: None,
                })
                .into_resource()
            }
            #[cfg(target_os = "linux")]
            VirtioBlkBounceDiskHandle::BlockDevice(file) => {
                validate_bounce_block_device(&file)?;
                disk_backend_resources::BlockDeviceDiskHandle { file }.into_resource()
            }
        };
        Ok(resolver.resolve(disk, input).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_serial;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vm_resource::Resource;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    async fn resolve_bounced_disk(
        disk: Resource<DiskHandleKind>,
        driver: DefaultDriver,
    ) -> anyhow::Result<ResolvedVirtioDevice> {
        resolve_disk(disk, driver, true, &guestmem::GuestMemory::allocate(4096)).await
    }

    async fn resolve_disk(
        disk: Resource<DiskHandleKind>,
        driver: DefaultDriver,
        force: bool,
        memory: &guestmem::GuestMemory,
    ) -> anyhow::Result<ResolvedVirtioDevice> {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut resolver = ResourceResolver::new();
        resolver.add_async_resolver::<VirtioDeviceHandle, _, VirtioBlkHandle, _>(VirtioBlkResolver);
        resolver.add_async_resolver::<DiskHandleKind, _, VirtioBlkBounceDiskHandle, _>(
            VirtioBlkResolver,
        );
        Ok(resolver
            .resolve(
                VirtioBlkHandle {
                    disk,
                    read_only: false,
                    serial: None,
                    bounce_io: force,
                    bounce_disk: None,
                }
                .into_resource(),
                VirtioResolveInput {
                    driver_source: &driver_source,
                    guest_memory: Some(memory),
                },
            )
            .await?)
    }

    #[async_test]
    async fn automatic_bounce_rejects_generic_resource_before_resolution(driver: DefaultDriver) {
        let memory = guestmem::GuestMemory::allocate(4096).copy_only_view();
        assert!(!memory.supports_kernel_lock());
        let disk =
            disk_backend_resources::FileDiskHandle(std::fs::File::open("Cargo.toml").unwrap())
                .into_resource();
        let error = resolve_disk(disk, driver, false, &memory)
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains(
                "bounce I/O requires a reviewed file, RAM, or Linux block-device resource"
            ),
            "{error:#}"
        );
    }

    #[async_test]
    async fn automatic_backing_does_not_admit_unreviewed_alternative(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let memory = guestmem::GuestMemory::allocate(4096).copy_only_view();
        let error = <VirtioBlkResolver as AsyncResolveResource<
            VirtioDeviceHandle,
            VirtioBlkHandle,
        >>::resolve(
            &VirtioBlkResolver,
            &ResourceResolver::new(),
            VirtioBlkHandle {
                disk: VirtioBlkBounceDiskHandle::Ram { len: 4096 }.into_resource(),
                read_only: false,
                serial: None,
                bounce_io: false,
                bounce_disk: Some(
                    disk_backend_resources::FileDiskHandle(
                        std::fs::File::open("Cargo.toml").unwrap(),
                    )
                    .into_resource(),
                ),
            },
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
                .contains("requires a reviewed file, RAM, or Linux block-device resource")
        );
    }

    #[async_test]
    async fn alternative_selection_follows_effective_memory_policy(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut resolver = ResourceResolver::new();
        resolver.add_async_resolver::<DiskHandleKind, _, VirtioBlkBounceDiskHandle, _>(
            VirtioBlkResolver,
        );
        for copy_only in [false, true] {
            for force in [false, true] {
                let memory = guestmem::GuestMemory::allocate(4096);
                let memory = if copy_only {
                    memory.copy_only_view()
                } else {
                    memory
                };
                let error = <VirtioBlkResolver as AsyncResolveResource<
                    VirtioDeviceHandle,
                    VirtioBlkHandle,
                >>::resolve(
                    &VirtioBlkResolver,
                    &resolver,
                    VirtioBlkHandle {
                        // Distinct admission errors identify the selected backing.
                        disk: VirtioBlkBounceDiskHandle::Ram { len: 0 }.into_resource(),
                        read_only: false,
                        serial: None,
                        bounce_io: force,
                        bounce_disk: Some(
                            disk_backend_resources::FileDiskHandle(
                                std::fs::File::open("Cargo.toml").unwrap(),
                            )
                            .into_resource(),
                        ),
                    },
                    VirtioResolveInput {
                        driver_source: &driver_source,
                        guest_memory: Some(&memory),
                    },
                )
                .await
                .err()
                .unwrap();
                let error = format!("{error:#}");
                if force || copy_only {
                    assert!(
                        error.contains(
                            "requires a reviewed file, RAM, or Linux block-device resource"
                        ),
                        "{error}"
                    );
                } else {
                    assert!(
                        error.contains("bounce RAM disk length must be a positive multiple of 512"),
                        "{error}"
                    );
                }
            }
        }
    }

    #[async_test]
    async fn direct_memory_does_not_require_bounce_resource(driver: DefaultDriver) {
        let memory = guestmem::GuestMemory::allocate(4096);
        assert!(memory.supports_kernel_lock());
        let disk =
            disk_backend_resources::FileDiskHandle(std::fs::File::open("Cargo.toml").unwrap())
                .into_resource();
        match resolve_disk(disk, driver, false, &memory).await {
            Ok(device) => assert!(device.0.supports_save_restore()),
            Err(error) => assert!(
                !format!("{error:#}").contains(
                    "bounce I/O requires a reviewed file, RAM, or Linux block-device resource"
                ),
                "{error:#}"
            ),
        }
    }

    #[async_test]
    async fn bounce_resolver_rejects_generic_file_before_backend_resolution(driver: DefaultDriver) {
        let disk =
            disk_backend_resources::FileDiskHandle(std::fs::File::open("Cargo.toml").unwrap())
                .into_resource();
        let error = resolve_bounced_disk(disk, driver).await.err().unwrap();
        assert!(
            format!("{error:#}").contains(
                "bounce I/O requires a reviewed file, RAM, or Linux block-device resource"
            ),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[async_test]
    async fn bounce_resolver_rejects_typed_append_file(driver: DefaultDriver) {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open("Cargo.toml")
            .unwrap();
        assert_eq!(
            validate_bounce_file(&file).unwrap_err().to_string(),
            "bounce I/O does not support O_APPEND file descriptors"
        );
        let error = resolve_bounced_disk(
            VirtioBlkBounceDiskHandle::File(file).into_resource(),
            driver,
        )
        .await
        .err()
        .unwrap();
        assert!(
            format!("{error:#}").contains("bounce I/O does not support O_APPEND file descriptors"),
            "{error:#}"
        );
    }

    #[test]
    fn bounce_rejects_generic_backing_resources() {
        validate_bounce_resource_id(VirtioBlkBounceDiskHandle::ID).unwrap();
        for id in ["file", "block", "layered", "nvme", "unknown"] {
            assert!(validate_bounce_resource_id(id).is_err(), "{id}");
        }
    }

    #[test]
    fn bounce_validates_opened_descriptor() {
        let file = std::fs::File::open("Cargo.toml").unwrap();
        validate_bounce_file(&file).unwrap();
        #[cfg(unix)]
        {
            let directory = std::fs::File::open(".").unwrap();
            assert!(validate_bounce_file(&directory).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounce_rejects_direct_io_descriptor() {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_DIRECT.bits())
            .open("Cargo.toml")
            .unwrap();
        assert!(validate_bounce_file(&file).is_err());
    }

    #[cfg(target_os = "linux")]
    #[async_test]
    async fn reviewed_block_resource_rejects_other_descriptor_types(driver: DefaultDriver) {
        for path in ["Cargo.toml", ".", "/dev/null"] {
            let file = std::fs::File::open(path).unwrap();
            assert!(validate_bounce_block_device(&file).is_err());
            let error = resolve_bounced_disk(
                VirtioBlkBounceDiskHandle::BlockDevice(file).into_resource(),
                driver.clone(),
            )
            .await
            .err()
            .unwrap();
            assert!(
                format!("{error:#}").contains("requires a Linux block device"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn serial_is_nul_padded() {
        assert_eq!(
            parse_serial("DATA-DISK".into()).unwrap(),
            *b"DATA-DISK\0\0\0\0\0\0\0\0\0\0\0"
        );
        assert!(parse_serial("\u{1f},[]".into()).is_ok());
    }

    #[test]
    fn invalid_serial_is_rejected() {
        assert!(parse_serial(String::new()).is_err());
        assert!(parse_serial("123456789012345678901".into()).is_err());
        assert!(parse_serial("non-ascii-\u{e9}".into()).is_err());
    }
}
