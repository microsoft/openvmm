// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 USB UHCI stub device.

use super::Piix4UsbUhciStub;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::piix4_uhci::Piix4PciUsbUhciStubDeviceHandle;
use std::convert::Infallible;
use vm_resource::AsyncResolveResource;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the PIIX4 USB UHCI stub device.
pub struct Piix4PciUsbUhciStubResolver;

declare_static_async_resolver! {
    Piix4PciUsbUhciStubResolver,
    (ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle>
    for Piix4PciUsbUhciStubResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Infallible;

    async fn resolve(
        &self,
        _resolver: &vm_resource::ResourceResolver,
        resource: Piix4PciUsbUhciStubDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // As per PIIX4 spec, UHCI sits at fixed BDF 00:07.2.
        input
            .configure
            .register_static_pci(resource.pci_bus_name.as_str(), (0, 7, 2));

        Ok(Piix4UsbUhciStub::new().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::pio::ExternallyManagedPortIoIntercepts;
    use chipset_device_resources::ConfigureChipsetDevice;
    use chipset_device_resources::LineSetId;
    use guestmem::GuestMemory;
    use pal_async::DefaultPool;
    use std::ops::RangeInclusive;
    use test_with_tracing::test;
    use vm_resource::ResourceResolver;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    #[derive(Default)]
    struct SpyConfigure {
        static_pci: Option<(String, (u8, u8, u8))>,
    }

    impl ConfigureChipsetDevice for SpyConfigure {
        fn new_line(&mut self, _id: LineSetId, _name: &str, _vector: u32) -> LineInterrupt {
            LineInterrupt::detached()
        }

        fn add_line_target(
            &mut self,
            _id: LineSetId,
            _source_range: RangeInclusive<u32>,
            _target_start: u32,
        ) {
        }

        fn register_static_pci(&mut self, bus_name: &str, bdf: (u8, u8, u8)) {
            self.static_pci = Some((bus_name.to_owned(), bdf));
        }

        fn omit_saved_state(&mut self) {}
    }

    #[test]
    fn resolver_registers_fixed_piix4_uhci_bdf_on_requested_bus() {
        let mut pool = DefaultPool::new();
        let driver = pool.driver();
        let vm_time_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = pool
            .run_until(vm_time_keeper.builder().build(&driver))
            .expect("vm time source should initialize");

        let guest_memory = GuestMemory::allocate(4096);
        let task_driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

        let mut configure = SpyConfigure::default();
        let mut register_mmio = ExternallyManagedMmioIntercepts;
        let mut register_pio = ExternallyManagedPortIoIntercepts;

        let resolver = Piix4PciUsbUhciStubResolver;
        let resource = Piix4PciUsbUhciStubDeviceHandle {
            pci_bus_name: "i440bx".to_owned(),
        };

        pool.run_until(async {
            resolver
                .resolve(
                    &ResourceResolver::new(),
                    resource,
                    ResolveChipsetDeviceHandleParams {
                        device_name: "piix4-usb-uhci-stub",
                        guest_memory: &guest_memory,
                        encrypted_guest_memory: &guest_memory,
                        vmtime: &vmtime,
                        is_restoring: false,
                        configure: &mut configure,
                        task_driver_source: &task_driver_source,
                        register_mmio: &mut register_mmio,
                        register_pio: &mut register_pio,
                    },
                )
                .await
                .expect("resolver should succeed");
        });

        assert_eq!(configure.static_pci, Some(("i440bx".to_owned(), (0, 7, 2))));
    }
}
