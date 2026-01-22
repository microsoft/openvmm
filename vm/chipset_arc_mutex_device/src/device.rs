// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A builder to streamline the construction of `Arc + Mutex` wrapped
//! [`ChipsetDevice`] instances.

use crate::services::ChipsetServices;
use arc_cyclic_builder::ArcCyclicBuilder;
use arc_cyclic_builder::ArcCyclicBuilderExt;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use closeable_mutex::CloseableMutex;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use tracing::instrument;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ServiceKind {
    Mmio,
    PortIo,
    Pci,
    PollDevice,
}

impl ServiceKind {
    fn supports_fn(self) -> &'static str {
        match self {
            ServiceKind::Mmio => "mmio",
            ServiceKind::PortIo => "pio",
            ServiceKind::Pci => "pci",
            ServiceKind::PollDevice => "poll_device",
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum AddDeviceErrorKind {
    #[error("could not construct device")]
    DeviceError(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error("device attmpted to use {0:?} services without overriding supports_{sup}", sup = .0.supports_fn())]
    DeviceMissingSupports(ServiceKind),
    #[error("chipset does not support {0:?} services (`ChipsetServices::supports_{sup}` returned `None`)", sup = .0.supports_fn())]
    ChipsetMissingSupports(ServiceKind),

    #[error("no pci bus address provided")]
    NoPciBusAddress,
}

impl AddDeviceErrorKind {
    pub(crate) fn with_dev_name(self, dev_name: Arc<str>) -> AddDeviceError {
        AddDeviceError {
            dev_name,
            inner: self,
        }
    }
}

/// Errors that may occur while adding a device to the chipset.
#[derive(Debug, Error)]
#[error("could not initialize {dev_name}")]
pub struct AddDeviceError {
    dev_name: Arc<str>,
    #[source]
    inner: AddDeviceErrorKind,
}

/// Additional trait implemented by Arc + CloseableMutex [`ChipsetServices`] that gives
/// the services an opportunity to perform any chipset-specific wiring of the
/// constructed `Arc<CloseableMutex<T: ChipsetDevice>>`.
///
/// This is a separate trait from [`ChipsetServices`] because it is specific to
/// the ArcMutex infrastructure.
pub trait ArcMutexChipsetServicesFinalize<T> {
    /// Called to finish wiring up the device after it has been completely
    /// constructed.
    fn finalize(self, dev: &Arc<CloseableMutex<T>>, name: Arc<str>);
}

/// A builder to streamline the construction of `Arc + CloseableMutex` wrapped
/// [`ChipsetDevice`] instances.
pub struct ArcMutexChipsetDeviceBuilder<S, T> {
    services: S,
    arc_builder: ArcCyclicBuilder<CloseableMutex<T>>,

    dev_name: Arc<str>,

    pci_addr: Option<(u8, u8, u8)>,
    external_pci: bool,
}

impl<S, T> ArcMutexChipsetDeviceBuilder<S, T>
where
    T: ChipsetDevice,
    S: ChipsetServices + ArcMutexChipsetServicesFinalize<T>,
{
    /// Create a new [`ArcMutexChipsetDeviceBuilder`]
    pub fn new(
        name: Arc<str>,
        new_device_services: impl FnOnce(Weak<CloseableMutex<dyn ChipsetDevice>>, Arc<str>) -> S,
    ) -> ArcMutexChipsetDeviceBuilder<S, T> {
        let arc_builder: ArcCyclicBuilder<CloseableMutex<T>> = Arc::new_cyclic_builder();
        let services = (new_device_services)(arc_builder.weak(), name.clone());

        ArcMutexChipsetDeviceBuilder {
            services,
            arc_builder,

            dev_name: name,

            pci_addr: None,
            external_pci: false,
        }
    }

    /// For PCI devices: place the device at the following PCI address
    pub fn with_pci_addr(mut self, bus: u8, device: u8, function: u8) -> Self {
        self.pci_addr = Some((bus, device, function));
        self
    }

    /// For PCI devices: do not register the device with the chipset's PCI
    /// services. This is used when the device is hooked up to a bus (such as a
    /// VPCI bus) outside of the chipset infrastructure.
    pub fn with_external_pci(mut self) -> Self {
        self.external_pci = true;
        self
    }

    fn inner_add(
        mut self,
        typed_dev: Result<T, AddDeviceError>,
    ) -> Result<Arc<CloseableMutex<T>>, AddDeviceError> {
        let mut typed_dev = typed_dev?;

        /// Cut down on boilerplate required to wire up devices to services,
        /// taking care of common failure modes while leaving the details of
        /// extra service-specific wiring up to the macro invocation.
        macro_rules! wire_up_service {
            (
                ($supports_fn:ident, $service_kind:expr);
                ($service:pat, $dev:pat) => $body:block
            ) => {
                match (self.services.$supports_fn(), typed_dev.$supports_fn()) {
                    (Some($service), Some($dev)) => $body,
                    (None, Some(_)) => {
                        return Err(AddDeviceErrorKind::ChipsetMissingSupports($service_kind)
                            .with_dev_name(self.dev_name))
                    }
                    (Some(service), None) => {
                        // check for faulty device impl
                        if service.is_being_used() {
                            return Err(AddDeviceErrorKind::DeviceMissingSupports($service_kind)
                                .with_dev_name(self.dev_name));
                        }
                    }
                    (None, None) => {}
                }
            };
        }

        wire_up_service! {
            (supports_mmio, ServiceKind::Mmio);
            (service, dev) => {
                // static mmio registration
                for (label, range) in dev.get_static_regions() {
                    service
                        .register_mmio()
                        .new_io_region(label, range.end() - range.start() + 1)
                        .map(*range.start());
                }
            }
        };

        wire_up_service! {
            (supports_pio, ServiceKind::PortIo);
            (service, dev) => {
                // static pio registration
                for (label, range) in dev.get_static_regions() {
                    service
                        .register_pio()
                        .new_io_region(label, range.end() - range.start() + 1)
                        .map(*range.start());
                }
            }
        };

        if !self.external_pci {
            wire_up_service! {
                (supports_pci, ServiceKind::Pci);
                (service, dev) => {
                    // static pci registration
                    let (bus, device, function) = match (self.pci_addr, dev.suggested_bdf()) {
                        (Some(override_bdf), Some(suggested_bdf)) => {
                            let (ob, od, of) = override_bdf;
                            let (sb, sd, sf) = suggested_bdf;
                            tracing::info!(
                                "overriding suggested bdf: using {:02x}:{:02x}:{} instead of {:02x}:{:02x}:{}",
                                ob, od, of,
                                sb, sd, sf
                            );
                            override_bdf
                        }
                        (None, Some(bdf)) | (Some(bdf), None) => bdf,
                        (None, None) => {
                            return Err(AddDeviceErrorKind::NoPciBusAddress.with_dev_name(self.dev_name))
                        }
                    };

                    service.register_static_pci(bus, device, function);
                }
            }
        }

        wire_up_service! {
            (supports_poll_device, ServiceKind::PollDevice);
            (service, _) => {
                service.register_poll();
            }
        }

        let dev = self.arc_builder.build(CloseableMutex::new(typed_dev));

        // Now ask the services to finish wiring up the device.
        self.services.finalize(&dev, self.dev_name);

        Ok(dev)
    }

    /// Construct a new device.
    ///
    /// If the device can fail during initialization, use
    /// [`try_add`](Self::try_add) instead.
    ///
    /// Includes some basic validation that returns an error if a device
    /// attempts to use a service without also implementing the service's
    /// corresponding `ChipsetDevice::supports_` method.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    #[expect(clippy::should_implement_trait)]
    pub fn add<F>(mut self, f: F) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: FnOnce(&mut S) -> T,
    {
        let dev = (f)(&mut self.services);
        self.inner_add(Ok(dev))
    }

    /// Just like [`add`](Self::add), except async.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    pub async fn add_async<F>(mut self, f: F) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: AsyncFnOnce(&mut S) -> T,
    {
        let dev = (f)(&mut self.services).await;
        self.inner_add(Ok(dev))
    }

    /// Just like [`add`](Self::add), except fallible.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    pub fn try_add<F, E>(mut self, f: F) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: FnOnce(&mut S) -> Result<T, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        let dev = match (f)(&mut self.services) {
            Ok(dev) => dev,
            Err(e) => {
                return Err(AddDeviceErrorKind::DeviceError(e.into()).with_dev_name(self.dev_name));
            }
        };
        self.inner_add(Ok(dev))
    }

    /// Just like [`try_add`](Self::try_add), except async.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    pub async fn try_add_async<F, E>(
        mut self,
        f: F,
    ) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: AsyncFnOnce(&mut S) -> Result<T, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        let dev = match (f)(&mut self.services).await {
            Ok(dev) => dev,
            Err(e) => {
                return Err(AddDeviceErrorKind::DeviceError(e.into()).with_dev_name(self.dev_name));
            }
        };
        self.inner_add(Ok(dev))
    }
}
