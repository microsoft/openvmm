// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to wire up [`ChipsetDevice`] instances to the
//! [`Chipset`](crate::Chipset).

use super::services::ArcMutexChipsetServices;
use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::VmmChipsetDevice;
use arc_cyclic_builder::ArcCyclicBuilder;
use arc_cyclic_builder::ArcCyclicBuilderExt;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use closeable_mutex::CloseableMutex;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use tracing::instrument;

#[derive(Debug, Error)]
pub(crate) enum AddDeviceErrorKind {
    #[error("could not construct device")]
    DeviceError(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error("no pci bus address provided")]
    NoPciBusAddress,
    #[error("error finalizing device")]
    Finalize(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
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
    /// The error type returned by the `finalize` method.
    type Error;

    /// Called to finish wiring up the device after it has been completely
    /// constructed.
    fn finalize(self, dev: &Arc<CloseableMutex<T>>, name: Arc<str>) -> Result<(), Self::Error>;
}

/// A builder to streamline the construction of `Arc + CloseableMutex` wrapped
/// `ChipsetDevice` instances.
pub struct ArcMutexChipsetDeviceBuilder<'a, 'b, T> {
    services: ArcMutexChipsetServices<'a, 'b>,
    arc_builder: ArcCyclicBuilder<CloseableMutex<T>>,

    dev_name: Arc<str>,

    pci_addr: Option<(u8, u8, u8)>,
    pci_bus_id: Option<BusIdPci>,
    pcie_port: Option<BusIdPcieDownstreamPort>,
    external_pci: bool,
}

impl<'a, 'b, T> ArcMutexChipsetDeviceBuilder<'a, 'b, T>
where
    T: VmmChipsetDevice,
{
    /// Create a new [`ArcMutexChipsetDeviceBuilder`]
    pub fn new(
        name: Arc<str>,
        new_device_services: impl FnOnce(
            Weak<CloseableMutex<T>>,
            Arc<str>,
        ) -> ArcMutexChipsetServices<'a, 'b>,
    ) -> Self {
        let arc_builder: ArcCyclicBuilder<CloseableMutex<T>> = Arc::new_cyclic_builder();
        let services = (new_device_services)(arc_builder.weak(), name.clone());

        ArcMutexChipsetDeviceBuilder {
            services,
            arc_builder,

            dev_name: name,

            pci_addr: None,
            pci_bus_id: None,
            pcie_port: None,
            external_pci: false,
        }
    }

    /// Omit device saved state. Be careful when using this! Currently only used
    /// for `MissingDev`!
    pub fn omit_saved_state(mut self) -> Self {
        self.services.omit_saved_state();
        self
    }

    /// For PCI devices: place the device at the following PCI address
    pub fn with_pci_addr(mut self, bus: u8, device: u8, function: u8) -> Self {
        self.pci_addr = Some((bus, device, function));
        self
    }

    /// For PCI devices: place the device on the specific bus
    pub fn on_pci_bus(mut self, id: BusIdPci) -> Self {
        self.pci_bus_id = Some(id);
        self
    }

    /// For PCIe devices: place the device on the specified downstream port
    pub fn on_pcie_port(mut self, id: BusIdPcieDownstreamPort) -> Self {
        self.pcie_port = Some(id);
        self
    }

    /// For PCI devices: do not register the device with any PCI bus. This is
    /// used when the device is hooked up to a bus (such as a VPCI bus) outside
    /// of the vmotherboard infrastructure.
    pub fn with_external_pci(mut self) -> Self {
        self.external_pci = true;
        self
    }

    fn inner_add(
        mut self,
        typed_dev: Result<T, AddDeviceError>,
    ) -> Result<Arc<CloseableMutex<T>>, AddDeviceError> {
        let mut typed_dev = typed_dev?;

        if let Some(dev) = typed_dev.supports_mmio() {
            // static mmio registration
            for (label, range) in dev.get_static_regions() {
                self.services
                    .register_mmio()
                    .new_io_region(label, range.end() - range.start() + 1)
                    .map(*range.start());
            }
        }

        if let Some(dev) = typed_dev.supports_pio() {
            // static pio registration
            for (label, range) in dev.get_static_regions() {
                self.services
                    .register_pio()
                    .new_io_region(label, range.end() - range.start() + 1)
                    .map(*range.start());
            }
        }

        if !self.external_pci {
            if let Some(dev) = typed_dev.supports_pci() {
                if self.pci_bus_id.is_some() && self.pcie_port.is_some() {
                    panic!(
                        "wiring error: invoked both `on_pci_bus` and `on_pcie_port` for `{}`",
                        self.dev_name
                    );
                }

                if let Some(bus_id_port) = self.pcie_port {
                    self.services.register_static_pcie(bus_id_port);
                } else {
                    // static pci registration
                    let bdf = match (self.pci_addr, dev.suggested_bdf()) {
                        (Some(override_bdf), Some(suggested_bdf)) => {
                            let (ob, od, of) = override_bdf;
                            let (sb, sd, sf) = suggested_bdf;
                            tracing::info!(
                                "overriding suggested bdf: using {:02x}:{:02x}:{} instead of {:02x}:{:02x}:{}",
                                ob,
                                od,
                                of,
                                sb,
                                sd,
                                sf
                            );
                            override_bdf
                        }
                        (None, Some(bdf)) | (Some(bdf), None) => bdf,
                        (None, None) => {
                            return Err(
                                AddDeviceErrorKind::NoPciBusAddress.with_dev_name(self.dev_name)
                            );
                        }
                    };

                    let bus_id = match self.pci_bus_id.take() {
                        Some(bus_id) => bus_id,
                        None => panic!(
                            "wiring error: did not invoke `on_pci_bus` for `{}`",
                            self.dev_name
                        ),
                    };

                    self.services.register_static_pci(bus_id, bdf);
                }
            }
        }

        let dev = self.arc_builder.build(CloseableMutex::new(typed_dev));

        // Now ask the services to finish wiring up the device.
        self.services
            .finalize(&dev, self.dev_name.clone())
            .map_err(|err| AddDeviceErrorKind::Finalize(err.into()).with_dev_name(self.dev_name))?;

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
        F: FnOnce(&mut ArcMutexChipsetServices<'a, 'b>) -> T,
    {
        let dev = (f)(&mut self.services);
        self.inner_add(Ok(dev))
    }

    /// Just like [`add`](Self::add), except async.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    pub async fn add_async<F>(mut self, f: F) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: AsyncFnOnce(&mut ArcMutexChipsetServices<'a, 'b>) -> T,
    {
        let dev = (f)(&mut self.services).await;
        self.inner_add(Ok(dev))
    }

    /// Just like [`add`](Self::add), except fallible.
    #[instrument(name = "add_device", skip_all, fields(device = self.dev_name.as_ref()))]
    pub fn try_add<F, E>(mut self, f: F) -> Result<Arc<CloseableMutex<T>>, AddDeviceError>
    where
        F: FnOnce(&mut ArcMutexChipsetServices<'a, 'b>) -> Result<T, E>,
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
        F: AsyncFnOnce(&mut ArcMutexChipsetServices<'a, 'b>) -> Result<T, E>,
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
