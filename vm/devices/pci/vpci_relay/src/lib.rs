// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Virtual PCI relay
//!
//! This module provides a virtual PCI relay for the OpenHCL paravisor. It
//! consumes VPCI buses from the host and relays them to the guest, filtering
//! them as needed.

#[cfg(target_os = "linux")]
pub mod linux_mmio;

// Exported to make it easier to define filters without explicitly pulling in
// `pci_core`.
pub use pci_core::spec::hwid::ClassCode;
pub use pci_core::spec::hwid::ProgrammingInterface;
pub use pci_core::spec::hwid::Subclass;

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use futures::StreamExt as _;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use openhcl_tdisp::TdispResourceValidationInterface;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::hwid::HardwareIds;
use state_unit::StateUnits;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::task::Waker;
use user_driver::DmaClient;
use virt::IsolationType;
use vmbus_client::driver::OpenParams;
use vmbus_server::Guid;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ChipsetDevices;
use vmotherboard::DynamicDeviceUnit;
use vpci_client::MemoryAccess;
use vpci_client::VpciClient;
use vpci_client::VpciDevice;
use vpci_client::VpciDeviceEject;
use vpci_client::tdisp::TdispVpciAttestationInterface;

/// TODO TDISP: Required for the tdisp crate to be built in the meantime.
#[expect(unused_imports)]
use tdisp::TdispHostDeviceInterface;
use tdisp::TdispIsolationReport;
use tdisp::TdispIsolationReporter;
use tdisp::TdispTdiState;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;

/// Trait for creating memory access instances.
pub trait CreateMemoryAccess: 'static + Send + Sync {
    /// Creates a new memory access instance for the given guest physical address.
    fn create_memory_access(&self, gpa: u64) -> anyhow::Result<Box<dyn MemoryAccess>>;
}

/// The size of the MMIO region required for each VPCI device.
pub const VPCI_RELAY_MMIO_PER_DEVICE: u64 = vpci_client::MMIO_SIZE;

/// Flags for controlling optional behavior of the VPCI relay.
#[derive(Inspect, Debug, Default, Copy, Clone)]
pub struct VpciRelayOptions {
    /// When set, the relay will exercise a mock TDISP flow for emulated TDISP
    /// devices produced by OpenVMM tests.
    pub test_tdisp_flow: bool,
}

/// Virtual PCI relay.
#[derive(Inspect)]
pub struct VpciRelay {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    dma_client: Arc<dyn DmaClient>,
    #[inspect(skip)]
    new_buses: Vec<vmbus_client::OfferInfo>,
    #[inspect(skip)]
    bus_recv: mesh::Receiver<vmbus_client::OfferInfo>,
    #[inspect(skip)]
    vmbus: Arc<vmbus_server::VmbusServerControl>,
    #[inspect(iter_by_key)]
    devices: slab::Slab<RelayedDevice>,
    mmio_range: MemoryRange,
    #[inspect(skip)]
    mmio_access: Box<dyn CreateMemoryAccess>,
    #[inspect(iter_by_index)]
    allowed_devices: Vec<AllowedDevice>,
    #[inspect(hex)]
    vtom: Option<u64>,
    isolation_type: IsolationType,
    options: VpciRelayOptions,
    #[inspect(skip)]
    resource_validator: Arc<dyn TdispResourceValidationInterface>,
}

#[derive(Inspect)]
struct RelayedDevice {
    bus_instance_id: Guid,
    bus_client: VpciClient,
    #[inspect(skip)]
    vpci_device: Arc<VpciDevice>,
    #[inspect(skip)]
    removed: VpciDeviceEject,
    #[inspect(skip)]
    bus_unit: DynamicDeviceUnit,
    #[inspect(skip)]
    device_unit: DynamicDeviceUnit,
    ready_to_remove: bool,
}

impl RelayedDevice {
    async fn remove(self) {
        // Tear down the guest-facing surface first so the guest can no
        // longer issue packets against the channel while we unbind the
        // TDI on the host side.
        self.bus_unit.remove().await;
        self.device_unit.remove().await;

        // Only devices that actually completed at least a Bind have a
        // TDI on the host side to unbind. Non-TDISP devices stay in
        // `Uninitialized` and must be left alone. `tdisp_unbind` on
        // them would return a host error.
        if self.vpci_device.tdisp_tdi_state().await != TdispTdiState::Uninitialized {
            if let Err(err) = self
                .vpci_device
                .tdisp_unbind(tdisp::TdispGuestUnbindReason::DeviceTeardown)
                .await
            {
                tracing::warn!(
                    bus_instance_id = %self.bus_instance_id,
                    error = &*err as &dyn std::error::Error,
                    "tdisp_unbind during relay teardown failed"
                );
            }
        }

        self.bus_client.shutdown().await;
    }
}

/// An allowed device description.
///
/// Fields that are `Some` must match the device being evaluated to be allowed.
#[derive(Inspect, Copy, Clone, Debug)]
pub struct AllowedDevice {
    /// The vendor ID of the device.
    #[inspect(hex)]
    pub vendor_id: Option<u16>,
    /// The device ID of the device.
    #[inspect(hex)]
    pub device_id: Option<u16>,
    /// The revision ID of the device.
    #[inspect(hex)]
    pub revision_id: Option<u8>,
    /// The programming interface of the device.
    pub prog_if: Option<ProgrammingInterface>,
    /// The subclass of the device.
    pub sub_class: Option<Subclass>,
    /// The base class of the device.
    pub base_class: Option<ClassCode>,
    /// The sub-vendor ID.
    #[inspect(hex)]
    pub sub_vendor_id: Option<u16>,
    /// The sub-system ID.
    #[inspect(hex)]
    pub sub_system_id: Option<u16>,
}

impl AllowedDevice {
    fn allows(&self, hw: &HardwareIds) -> bool {
        let Self {
            vendor_id,
            device_id,
            revision_id,
            prog_if,
            sub_class,
            base_class,
            sub_vendor_id,
            sub_system_id,
        } = *self;
        vendor_id.is_none_or(|x| x == hw.vendor_id)
            && device_id.is_none_or(|x| x == hw.device_id)
            && revision_id.is_none_or(|x| x == hw.revision_id)
            && prog_if.is_none_or(|x| x == hw.prog_if)
            && sub_class.is_none_or(|x| x == hw.sub_class)
            && base_class.is_none_or(|x| x == hw.base_class)
            && sub_vendor_id.is_none_or(|x| x == hw.type0_sub_vendor_id)
            && sub_system_id.is_none_or(|x| x == hw.type0_sub_system_id)
    }
}

impl VpciRelay {
    /// Creates a new VPCI relay.
    pub fn new(
        driver_source: VmTaskDriverSource,
        offers: vmbus_client::ConnectResult,
        vmbus: Arc<vmbus_server::VmbusServerControl>,
        dma_client: Arc<dyn DmaClient>,
        mmio_range: MemoryRange,
        mmio_access: Box<dyn CreateMemoryAccess>,
        resource_validator: Arc<dyn TdispResourceValidationInterface>,
        isolation_type: IsolationType,
        vtom: Option<u64>,
        options: VpciRelayOptions,
    ) -> Self {
        // Setup test-specific values since TDISP tests don't necessarily take place inside a CVM runner.
        let target_isolation_type = if options.test_tdisp_flow {
            IsolationType::Snp
        } else {
            isolation_type
        };

        let target_vtom = if options.test_tdisp_flow {
            Some(0x400000000000) // For testing, we can just use VTOM value we expect from most SNP platforms.
        } else {
            vtom
        };

        Self {
            driver_source,
            dma_client,
            new_buses: offers.offers,
            bus_recv: offers.offer_recv,
            vmbus,
            devices: slab::Slab::new(),
            mmio_range,
            mmio_access,
            allowed_devices: Vec::new(),
            vtom: target_vtom,
            isolation_type: target_isolation_type,
            resource_validator,
            options,
        }
    }

    /// Adds an allowed device to the list. If one of the hardware ID is `!0`
    /// then it is treated as a wildcard.
    ///
    /// Note that if no devices are on the list, then all devices are allowed.
    pub fn add_allowed_device(&mut self, dev: AllowedDevice) {
        self.allowed_devices.push(dev);
    }

    /// Wait for the relay to be ready. This might never return. This call is cancellable.
    pub async fn wait_ready(&mut self) {
        poll_fn(|cx| {
            if !self.new_buses.is_empty() {
                return Poll::Ready(());
            }
            if self.devices.iter_mut().any(|(_, dev)| {
                let p = dev.ready_to_remove || dev.removed.poll_next_unpin(cx).is_ready();
                if p {
                    dev.ready_to_remove = true;
                }
                p
            }) {
                return Poll::Ready(());
            }
            if let Poll::Ready(Some(bus)) = self.bus_recv.poll_next_unpin(cx) {
                self.new_buses.push(bus);
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await
    }

    /// Process any waiting activity. This call is not cancellable.
    pub async fn process(
        &mut self,
        chipset: &ChipsetDevices,
        units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        let mut i = 0;
        while i < self.devices.len() {
            if self.devices[i].ready_to_remove {
                let dev = self.devices.remove(i);
                dev.remove().await;
            } else {
                i += 1;
            }
        }
        while let Some(bus) = self.new_buses.pop() {
            self.relay_vpci_bus(chipset, units, bus).await?;
        }
        Ok(())
    }

    async fn relay_vpci_bus(
        &mut self,
        chipset: &ChipsetDevices,
        state_units: &mut StateUnits,
        offer_info: vmbus_client::OfferInfo,
    ) -> anyhow::Result<()> {
        let entry = self.devices.vacant_entry();
        if (entry.key() as u64 + 1) * vpci_client::MMIO_SIZE > self.mmio_range.len() {
            anyhow::bail!("not enough MMIO space left");
        }

        let instance_id = offer_info.offer.instance_id;

        let mmio = self.mmio_access.create_memory_access(
            self.mmio_range.start() + (entry.key() as u64) * vpci_client::MMIO_SIZE,
        )?;

        let channel = vmbus_client::driver::open_channel(
            self.driver_source.simple(),
            offer_info,
            OpenParams {
                ring_pages: 20,
                ring_offset_in_pages: 10,
            },
            self.dma_client.as_ref(),
        )
        .await?;

        // FUTURE: handle more than one device. Note, though, that Hyper-V
        // doesn't really do this in practice.
        let (devices, _devices_recv) = mesh::channel();
        let (vpci_client, devices) =
            VpciClient::connect(self.driver_source.simple(), channel, mmio, devices).await?;

        let Some(vpci_device) = devices.into_iter().next() else {
            tracing::info!(%instance_id, "no device on VPCI bus");
            return Ok(());
        };

        let hw_ids = vpci_device.hw_ids();

        if !self.allowed_devices.is_empty()
            && !self.allowed_devices.iter().any(|d| d.allows(hw_ids))
        {
            let prog_if = hw_ids.prog_if;
            let sub_class = hw_ids.sub_class;
            let base_class = hw_ids.base_class;
            tracing::warn!(
                %instance_id,
                vendor_id = hw_ids.vendor_id,
                device_id = hw_ids.device_id,
                ?prog_if,
                ?sub_class,
                ?base_class,
                "device not allowed on VPCI bus"
            );
            return Ok(());
        }

        tracing::info!(%instance_id, vendor_id = hw_ids.vendor_id, device_id = hw_ids.device_id, "vpci relay device arrived");

        let (vpci_device, removed) = vpci_device
            .init(
                self.resource_validator.clone(),
                self.isolation_type,
                self.vtom.unwrap_or(0),
                hvdef::Vtl::Vtl0,
            )
            .await
            .context("failed to initialize vpci device")?;
        let vpci_device = Arc::new(vpci_device);

        // The host gets to decide if a device is TDISP capable or not
        let mut tdisp_capable = false;

        // If testing the mock TDISP flow...
        if self.options.test_tdisp_flow {
            Self::tdisp_test_mock_flow(vpci_device.clone())
                .await
                .expect("failed to exercise TDISP flow test");
        } else {
            // Probe TDISP capability without attesting.
            match vpci_device.tdisp_query_capabilities().await {
                Ok(_) => {
                    tdisp_capable = true;
                    tracing::info!(
                        %instance_id,
                        "TDISP capable device; deferring attestation until first guest interaction"
                    );
                }
                Err(e) => {
                    tracing::info!(
                        %instance_id,
                        failure_reason = ?e,
                        "TDISP not supported or failed to query capabilities"
                    );
                }
            }
        }

        let device_name = format!("assigned_device:vpci-{instance_id}");
        let (device_unit, device) = chipset
            .add_dyn_device(&self.driver_source, state_units, device_name, async |_| {
                Ok(RelayedVpciDevice {
                    device: vpci_device.clone(),
                    pending: None,
                    waker: Waker::noop().clone(),
                    tdisp_capable,
                })
            })
            .await?;

        let interrupt_mapper = VpciInterruptMapper::new(vpci_device.clone());

        let (bus_unit, _) = {
            let vpci_bus_name = format!("vpci:{instance_id}");
            chipset
                .add_dyn_device(
                    &self.driver_source,
                    state_units,
                    vpci_bus_name,
                    async |mmio| {
                        let bus = vpci::bus::VpciBus::new(
                            &self.driver_source,
                            vpci::bus::VpciBusConfig {
                                instance_id,
                                vtom: self.vtom,
                                vnode: None,
                            },
                            device,
                            mmio,
                            self.vmbus.as_ref(),
                            interrupt_mapper,
                        )
                        .await?;

                        anyhow::Ok(bus)
                    },
                )
                .await?
        };

        entry.insert(RelayedDevice {
            bus_instance_id: instance_id,
            bus_client: vpci_client,
            vpci_device: vpci_device.clone(),
            removed,
            bus_unit,
            device_unit,
            ready_to_remove: false,
        });

        state_units.start_stopped_units().await;
        Ok(())
    }

    /// Exercises a mocked TDISP flow for emulated TDISP devices produced by OpenVMM tests.
    async fn tdisp_test_mock_flow(device: Arc<VpciDevice>) -> anyhow::Result<()> {
        // For now, exercise just the "get device interface" flow and ensure that the device responds as
        // TDISP capable and with the right mocked device information.

        tracing::info!(
            "tdisp_test_mock_flow: exercising TDISP flow because OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST was set"
        );

        assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Uninitialized);

        let device_interface_info = device
            .tdisp_get_device_interface_info(TDISP_MOCK_GUEST_PROTOCOL)
            .await
            .context("tdisp_test_mock_flow: failed to get device interface info over vpci")?;

        tracing::info!(
            "tdisp_test_mock_flow: device interface info: {:?}",
            device_interface_info
        );

        assert_eq!(
            device_interface_info.guest_protocol_type,
            TDISP_MOCK_GUEST_PROTOCOL as i32
        );
        assert_eq!(device_interface_info.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
        assert_eq!(
            device_interface_info.supported_features,
            TDISP_MOCK_SUPPORTED_FEATURES
        );
        assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Unlocked);

        Self::tdisp_test_mock_attest_flow(device.clone())
            .await
            .context("tdisp_test_mock_flow: failed to exercise TDISP attestation flow")?;

        Ok(())
    }

    async fn tdisp_test_mock_attest_flow(device: Arc<VpciDevice>) -> anyhow::Result<()> {
        #[cfg(feature = "dev_snp_ohcl_tio_support")]
        let tdisp_tio_flow_enabled = true;
        #[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
        let tdisp_tio_flow_enabled = false;

        if tdisp_tio_flow_enabled {
            // Ensure the device appears to be tdisp capable
            let tdisp_capabilities = device
                .tdisp_query_capabilities()
                .await
                .context("tdisp_test_mock_flow: failed to query TDISP capabilities over vpci")?;

            assert_eq!(
                tdisp_capabilities.guest_protocol_type,
                TDISP_MOCK_GUEST_PROTOCOL as i32
            );
            assert_eq!(tdisp_capabilities.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
            assert_eq!(
                tdisp_capabilities.supported_features,
                TDISP_MOCK_SUPPORTED_FEATURES
            );
            assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Unlocked);

            // If the above interface works, try to attest the device through the TDISP flow and ensure that it succeeds.
            device
                .tdisp_attest_device(tdisp_capabilities)
                .await
                .context("tdisp_test_mock_flow: failed to attest device over vpci")?;

            assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Run);

            Ok(())
        } else {
            tracing::warn!(
                "tdisp_test_mock_attest_flow: skipping attestation flow because dev_snp_ohcl_tio_support feature is not enabled"
            );
            Ok(())
        }
    }
}

#[derive(InspectMut)]
struct RelayedVpciDevice {
    #[inspect(flatten)]
    device: Arc<VpciDevice>,

    /// In-flight deferred config space write. Driven by [`PollDevice`].
    #[inspect(skip)]
    pending: Option<(
        DeferredWrite,
        Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
    )>,

    /// Waker captured from the most recent `PollDevice::poll_device` call.
    /// We wake it from `pci_cfg_write` when we install a new pending future
    /// so the chipset device unit re-polls us.
    #[inspect(skip)]
    waker: Waker,

    /// Is the device TDISP capable?
    tdisp_capable: bool,
}

impl ChipsetDevice for RelayedVpciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_tdisp_isolation(&mut self) -> Option<&mut dyn TdispIsolationReporter> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for RelayedVpciDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.waker = cx.waker().clone();
        if let Some((_, fut)) = self.pending.as_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                // Future done; complete the deferred write so the bus can
                // continue draining any queued config writes.
                let (deferred, _) = self.pending.take().expect("just checked");
                deferred.complete();
            }
        }
    }
}

impl TdispIsolationReporter for RelayedVpciDevice {
    // Builds a report of what device resources for vpci device in a CVM are isolated or shared.
    fn tdisp_isolation_report(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TdispIsolationReport> + Send + 'static>> {
        let device = self.device.clone();
        let tdisp_capable = self.tdisp_capable;

        Box::pin(async move {
            // Whether the device is TDISP capable at all is decided once, when
            // the host offers the device, so answer that here rather than
            // asking the client. Everything else, including attesting when the
            // TDI is `Unlocked`, is the client's job.
            if !tdisp_capable {
                return TdispIsolationReport::NotTdispCapable;
            }

            device.tdisp_isolation_snapshot().await
        })
    }
}

impl PciConfigSpace for RelayedVpciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        self.device.read_cfg(offset, value);
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        // Only a command register write that flips the MMIO-enable bit needs
        // async TDISP work. Everything else is a synchronous pass-through.
        //
        // This matters beyond efficiency: `probe_bar_masks` sizes the BARs from
        // a synchronous context with no executor available, so it cannot honor
        // a deferred write.
        if !self.tdisp_capable || HeaderType00(offset) != HeaderType00::STATUS_COMMAND {
            self.device.write_cfg(offset, value);
            return IoResult::Ok;
        }

        // Detect the MMIO-enable edge BEFORE issuing the write so we can
        // dispatch the correct TDISP notification.
        //
        // The STATUS_COMMAND dword packs the 16-bit Command register in the low
        // two bytes and the 16-bit Status register in the high two bytes. Only
        // the Command register is relevant here, so mask off the Status half
        // before truncating to `u16`.
        use pci_core::spec::cfg_space::Command;
        let mut current = 0;
        self.device.read_cfg(
            offset,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut current),
        );
        let prev = Command::from((current & 0xffff) as u16).mmio_enabled();
        // `merge` honors the byte enables, so a partial write that leaves the
        // command register untouched yields `next` equal to `prev`.
        let next = Command::from((value.merge(current) & 0xffff) as u16).mmio_enabled();
        match (prev, next) {
            (false, true) => {}
            (true, false) => {
                // Once MMIO is on the TDI is bound and its ranges have been
                // unblocked and accepted into the guest. Drop the write rather
                // than letting the guest walk that back: the disable edge would
                // unbind the device and re-block every range.
                tracing::warn!(
                    ?offset,
                    ?value,
                    "dropping a config space write that would disable MMIO; the command \
                     register does not transition back to off once it is on"
                );
                return IoResult::Ok;
            }
            // No MMIO edge, so there is no TDISP notification to dispatch.
            _ => {
                self.device.write_cfg(offset, value);
                return IoResult::Ok;
            }
        }

        let device = self.device.clone();
        let fut = Box::pin(async move {
            // Attest while the command register is still off.
            // `tdisp_on_device_activate` enables the command register itself
            // once attestation succeeds, so the BARs are mapped before it
            // notifies TDISP of the MMIO ranges.
            if !device.tdisp_on_device_activate(value).await {
                // The command register is left off if attestation failed.
                tracing::warn!("TDISP attestation failed. Not enabling STATUS_COMMAND.");
            }
        });

        // Overwriting an in-flight deferral would drop its `DeferredWrite`,
        // which the caller sees as `IoError::NoResponse`. Every caller waits for
        // its own deferred write to complete, so this should not happen.
        debug_assert!(
            self.pending.is_none(),
            "config space write deferred while another deferred write is in flight"
        );

        let (write, token) = chipset_device::io::deferred::defer_write();
        self.pending = Some((write, fut));
        self.waker.wake_by_ref();
        IoResult::Defer(token)
    }
}

impl ChangeDeviceState for RelayedVpciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl SaveRestore for RelayedVpciDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}
