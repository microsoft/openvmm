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

mod tdispmock;
mod tests;

// Exported to make it easier to define filters without explicitly pulling in
// `pci_core`.
pub use pci_core::spec::hwid::ClassCode;
pub use pci_core::spec::hwid::ProgrammingInterface;
pub use pci_core::spec::hwid::Subclass;

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use futures::StreamExt as _;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use openhcl_tdisp::new_resource_validator;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::hwid::HardwareIds;
use state_unit::StateUnits;
use std::collections::VecDeque;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::task::Waker;
use tdisp::TdispIsolationReport;
use tdisp::TdispRelayedDeviceTarget;
use tdisp::TdispTdiState;
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

/// Trait for creating memory access instances.
pub trait CreateMemoryAccess: 'static + Send + Sync {
    /// Creates a new memory access instance for the given guest physical address.
    fn create_memory_access(&self, gpa: u64) -> anyhow::Result<Box<dyn MemoryAccess>>;
}

/// The size of the MMIO region required for each VPCI device.
pub const VPCI_RELAY_MMIO_PER_DEVICE: u64 = vpci_client::MMIO_SIZE;

/// Size and alignment of the window the mocked TDISP flow programs into a
/// device BAR so that it can reach the device's registers. Only reserved when
/// that flow is enabled, and large enough for the BARs the emulated test
/// devices implement.
const MOCK_BAR_MMIO_SIZE: u64 = 0x10000;

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
    /// Base of the window the mocked TDISP flow programs into a device BAR,
    /// carved out of `mmio_range` and never handed to a device's config space.
    /// Only set when that flow is enabled and the range had room for it.
    #[inspect(hex)]
    mock_bar_mmio: Option<u64>,
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

        // Unbind any TDI state if the device is a TDISP device.
        if self.vpci_device.tdisp_tdi_state().await != TdispTdiState::Unlocked {
            self.vpci_device
                .tdisp_unbind(tdisp::TdispGuestUnbindReason::DeviceTeardown)
                .await;
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

        // The mocked flow needs an address it can program into a device BAR and
        // then reach, and no guest has assigned any BARs by the time it runs.
        // Take an aligned block off the top of the relay's own MMIO and keep it
        // away from the per-device config space windows below.
        let (mmio_range, mock_bar_mmio) = if options.test_tdisp_flow {
            match Self::reserve_mock_bar_mmio(mmio_range) {
                Some((rest, mock)) => (rest, Some(mock)),
                None => {
                    tracing::warn!(
                        ?mmio_range,
                        "not enough relay MMIO to reserve a window for the mocked TDISP flow"
                    );
                    (mmio_range, None)
                }
            }
        } else {
            (mmio_range, None)
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
            options,
            mock_bar_mmio,
        }
    }

    /// Splits an aligned block off the end of `mmio_range` for the mocked TDISP
    /// flow to program into a device BAR, returning the rest of the range and
    /// the block's base address.
    ///
    /// Returns `None` when the range cannot give up an aligned block of that
    /// size, in which case the caller keeps the whole range and the mocked flow
    /// has no window to use.
    ///
    /// * `mmio_range` - The relay's MMIO range, which otherwise supplies one
    ///   config space window per device.
    fn reserve_mock_bar_mmio(mmio_range: MemoryRange) -> Option<(MemoryRange, u64)> {
        let end = mmio_range.end() & !(MOCK_BAR_MMIO_SIZE - 1);
        let base = end.checked_sub(MOCK_BAR_MMIO_SIZE)?;
        if base < mmio_range.start() {
            return None;
        }
        Some((MemoryRange::new(mmio_range.start()..base), base))
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

        // Create a TDISP platform validator based on the environment the relay
        // is running in. Validators take care of platform firmware operations
        // specific to the isolation technology in use. Test environments use
        // mocked firmware interfaces.
        let resource_validator =
            new_resource_validator(self.isolation_type, self.vtom, self.options.test_tdisp_flow)
                .context("failed to create a TDISP resource validator")?;

        let (vpci_device, removed) = vpci_device
            .init(
                resource_validator,
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
            let bar_mmio = self
                .mock_bar_mmio
                .expect("the mocked TDISP flow needs a reserved MMIO window");
            tdispmock::run_test_flow(vpci_device.clone(), self.mmio_access.as_ref(), bar_mmio)
                .await
                .expect("failed to exercise TDISP flow test");

            // Do not mark tdisp_capable = true because the test is already done.
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
                    queued: VecDeque::new(),
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
}

#[derive(InspectMut)]
struct RelayedVpciDevice {
    #[inspect(flatten)]
    device: Arc<VpciDevice>,

    /// The TDISP operation currently in flight, if any, paired with the config
    /// space write that started it. While this is set, no config space write
    /// reaches the device.
    #[inspect(skip)]
    pending: Option<(
        DeferredWrite,
        Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
    )>,

    /// Config space writes that arrived while an async operation was in flight,
    /// in arrival order. Only ever non-empty while an operation is in flight,
    /// so its depth shows how many callers a slow operation is holding up.
    #[inspect(with = "|x| x.len()")]
    queued: VecDeque<QueuedWrite>,

    /// Waker captured from the most recent poll, used to ask the device unit to
    /// poll this device again once an async operation has been started.
    #[inspect(skip)]
    waker: Waker,

    /// Is the device TDISP capable?
    tdisp_capable: bool,
}

/// A config space write held off because a TDISP operation was in flight.
struct QueuedWrite {
    /// The DWORD-aligned offset in config space the write targets.
    offset: u16,
    /// The value and byte enables to write.
    value: ByteEnabledDwordWrite,
    /// The write to complete once this has been applied to the device, or, when
    /// applying it starts another TDISP operation, once that operation ends.
    deferred: DeferredWrite,
}

/// The result of applying a config space write that had no TDISP operation
/// ahead of it.
enum CfgWriteOutcome {
    /// The write reached the device and needs nothing further.
    Complete,
    /// The write was deferred by cfg handling .The future carries out async
    /// work required. Other guest VPs are blocked from writing to cfg space
    /// while async work is dispatched and writes are instead queued for later
    /// processing.
    Started(Pin<Box<dyn Future<Output = ()> + Send + Sync>>),
}

impl RelayedVpciDevice {
    /// Applies a config space write to a relayed VPCI device. Special handling
    /// for TDISP devices might cause asynchronous operations to be initiated.
    ///
    /// `offset` is the DWORD-aligned offset in config space the write targets,
    /// and `value` the value and byte enables to write.
    fn apply_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> CfgWriteOutcome {
        // Only a command register write that flips the MMIO-enable bit needs
        // async TDISP work. Everything else is a synchronous pass-through.
        if !self.tdisp_capable || HeaderType00(offset) != HeaderType00::STATUS_COMMAND {
            self.device.write_cfg(offset, value);
            return CfgWriteOutcome::Complete;
        }

        // Detect the MMIO-enable edge on the command register BEFORE issuing
        // the write so we can dispatch the correct TDISP notification.
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
            // MMIO turning on. Attest before the guest can reach the BARs.
            // Activation writes the command register itself once attestation
            // succeeds, so the BARs are mapped before the MMIO ranges are
            // unblocked.
            //
            // If BARs are written to during or after this process, they will
            // only affect the shadow BARs and not the real device BARs. This
            // ensures misbehaving guests cannot remap their private sections
            // once attestation is complete.
            (false, true) => {
                let device = self.device.clone();
                CfgWriteOutcome::Started(Box::pin(async move {
                    if !device.tdisp_on_device_activate(value).await {
                        tracing::warn!(
                            "TDISP attestation failed, leaving the command register off"
                        );
                    }
                }))
            }
            // MMIO turning off. Tear the TDI back down. Deactivation leaves the
            // command register in its off state and unmaps all private BARs.
            (true, false) => {
                let device = self.device.clone();
                CfgWriteOutcome::Started(Box::pin(async move {
                    device.tdisp_on_device_deactivate().await;
                }))
            }
            // No MMIO edge, just pass through.
            (false, false) | (true, true) => {
                self.device.write_cfg(offset, value);
                CfgWriteOutcome::Complete
            }
        }
    }

    /// Makes `fut` the in-flight TDISP operation and asks the device unit to
    /// poll this device so that it starts making progress.
    ///
    /// `deferred` is the config space write that waits on the operation, and
    /// `fut` the operation itself.
    fn start_operation(
        &mut self,
        deferred: DeferredWrite,
        fut: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
    ) {
        // Every path here has just observed or made `pending` empty, so this
        // cannot displace an operation that is still running.
        assert!(
            self.pending.is_none(),
            "TDISP operation started while another was in flight"
        );
        self.pending = Some((deferred, fut));
        self.waker.wake_by_ref();
    }

    /// Applies the writes that queued up behind a TDISP operation, in arrival
    /// order, stopping at the first one that starts another operation.
    ///
    /// Must only be called with no operation in flight.
    fn drain_queued(&mut self) {
        while let Some(QueuedWrite {
            offset,
            value,
            deferred,
        }) = self.queued.pop_front()
        {
            match self.apply_cfg_write(offset, value) {
                CfgWriteOutcome::Complete => deferred.complete(),
                CfgWriteOutcome::Started(fut) => {
                    // The rest of the queue stays put, behind this operation.
                    self.start_operation(deferred, fut);
                    return;
                }
            }
        }
    }
}

impl ChipsetDevice for RelayedVpciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_tdisp_relay(&mut self) -> Option<&mut dyn TdispRelayedDeviceTarget> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for RelayedVpciDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.waker = cx.waker().clone();
        while let Some((_, fut)) = self.pending.as_mut() {
            if fut.as_mut().poll(cx).is_pending() {
                break;
            }

            // Keep queueing any deferred writes that are ready and re-poll them.
            let (deferred, _) = self.pending.take().expect("just checked");
            deferred.complete();
            self.drain_queued();
        }
    }
}

impl TdispRelayedDeviceTarget for RelayedVpciDevice {
    // Builds a report of what device resources for vpci device in a CVM are isolated or shared.
    fn tdisp_isolation_report(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TdispIsolationReport> + Send + 'static>> {
        let device = self.device.clone();
        let tdisp_capable = self.tdisp_capable;

        Box::pin(async move {
            // If the device is not TDISP capable, return early with an invalid report.
            if !tdisp_capable {
                return TdispIsolationReport::NotTdispCapable;
            }

            // This might fire an attestation flow if it hasn't already happened yet.
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
        // An asynchronous operation has to run to completion with nothing else
        // touching the device's config space. Writes arriving behind an already
        // queued write must wait and be applied in the order they arrived. The
        // chipset drops the device lock before waiting on a deferred access, so
        // several VPs, plus the VPCI channel worker, can be in here at once.
        if self.pending.is_some() || !self.queued.is_empty() {
            let (deferred, token) = defer_write();
            self.queued.push_back(QueuedWrite {
                offset,
                value,
                deferred,
            });
            return IoResult::Defer(token);
        }

        match self.apply_cfg_write(offset, value) {
            CfgWriteOutcome::Complete => IoResult::Ok,
            CfgWriteOutcome::Started(fut) => {
                let (deferred, token) = defer_write();
                self.start_operation(deferred, fut);
                IoResult::Defer(token)
            }
        }
    }
}

impl ChangeDeviceState for RelayedVpciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {
        // Nothing polls this device while it is stopped, so finish asynchronous
        // operations and everything queued behind them here. Otherwise the callers
        // waiting on those writes would be left waiting on a completion that
        // never comes.
        while let Some((deferred, fut)) = self.pending.take() {
            fut.await;
            deferred.complete();
            self.drain_queued();
        }
    }

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
