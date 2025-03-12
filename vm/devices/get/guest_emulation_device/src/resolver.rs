// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::GuestEmulationDevice;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use get_protocol::SecureBootTemplateType;
use get_resources::ged::GuestEmulationDeviceHandle;
use get_resources::ged::GuestFirmwareConfig;
use get_resources::ged::GuestSecureBootTemplateType;
use get_resources::ged::PcatBootDevice;
use get_resources::ged::UefiConsoleMode;
use power_resources::PowerRequestHandleKind;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

pub struct GuestEmulationDeviceResolver;

declare_static_async_resolver! {
    GuestEmulationDeviceResolver,
    (VmbusDeviceHandleKind, GuestEmulationDeviceHandle),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to resolve framebuffer")]
    Framebuffer(#[source] ResolveError),
    #[error("failed to resolve power request")]
    Power(#[source] ResolveError),
    #[error("failed to resolve vmgs disk")]
    Vmgs(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, GuestEmulationDeviceHandle>
    for GuestEmulationDeviceResolver
{
    type Output = ResolvedVmbusDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: GuestEmulationDeviceHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let framebuffer_control = if let Some(framebuffer) = resource.framebuffer {
            Some(
                resolver
                    .resolve(framebuffer, ())
                    .await
                    .map_err(Error::Framebuffer)?
                    .0,
            )
        } else {
            None
        };

        let halt = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(Error::Power)?;

        let vmgs_disk = if let Some(disk) = resource.vmgs_disk {
            Some(
                resolver
                    .resolve(
                        disk,
                        ResolveDiskParameters {
                            read_only: false,
                            _async_trait_workaround: &(),
                        },
                    )
                    .await
                    .map_err(Error::Vmgs)?
                    .0,
            )
        } else {
            None
        };

        let device = GuestEmulationDevice::new(
            crate::GuestConfig {
                firmware: match resource.firmware {
                    GuestFirmwareConfig::Uefi {
                        enable_vpci_boot,
                        firmware_debug,
                        disable_frontpage,
                        console_mode,
                    } => crate::GuestFirmwareConfig::Uefi {
                        enable_vpci_boot,
                        firmware_debug,
                        disable_frontpage,
                        console_mode: match console_mode {
                            UefiConsoleMode::Default => get_protocol::UefiConsoleMode::DEFAULT,
                            UefiConsoleMode::COM1 => get_protocol::UefiConsoleMode::COM1,
                            UefiConsoleMode::COM2 => get_protocol::UefiConsoleMode::COM2,
                            UefiConsoleMode::None => get_protocol::UefiConsoleMode::NONE,
                        },
                    },
                    GuestFirmwareConfig::Pcat { boot_order } => crate::GuestFirmwareConfig::Pcat {
                        boot_order: boot_order.map(|x| match x {
                            PcatBootDevice::Floppy => {
                                get_protocol::dps_json::PcatBootDevice::Floppy
                            }
                            PcatBootDevice::HardDrive => {
                                get_protocol::dps_json::PcatBootDevice::HardDrive
                            }
                            PcatBootDevice::Optical => {
                                get_protocol::dps_json::PcatBootDevice::Optical
                            }
                            PcatBootDevice::Network => {
                                get_protocol::dps_json::PcatBootDevice::Network
                            }
                        }),
                    },
                },
                com1: resource.com1,
                com2: resource.com2,
                vmbus_redirection: resource.vmbus_redirection,
                enable_tpm: resource.enable_tpm,
                vtl2_settings: resource.vtl2_settings,
                secure_boot_enabled: resource.secure_boot_enabled,
                secure_boot_template: match resource.secure_boot_template {
                    GuestSecureBootTemplateType::None => {
                        SecureBootTemplateType::SECURE_BOOT_DISABLED
                    }
                    GuestSecureBootTemplateType::MicrosoftWindows => {
                        SecureBootTemplateType::MICROSOFT_WINDOWS
                    }
                    GuestSecureBootTemplateType::MicrosoftUefiCertificateAuthoritiy => {
                        SecureBootTemplateType::MICROSOFT_UEFI_CERTIFICATE_AUTHORITY
                    }
                },
                enable_battery: resource.enable_battery,
            },
            halt,
            resource.firmware_event_send,
            resource.guest_request_recv,
            framebuffer_control,
            vmgs_disk,
        );
        Ok(SimpleDeviceWrapper::new(input.driver_source.simple(), device).into())
    }
}
