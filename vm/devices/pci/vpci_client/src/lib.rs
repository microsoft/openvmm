// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Client driver for VPCI (Virtual PCI) buses and devices.
//!
//! This implementation uses the configuration space-based interface for
//! resource and power management, like Linux does, as opposed to the
//! message-based interface, like Windows does.

pub mod tdisp;
mod tests;

pub use tdisp::VpciClientTdispState;

use anyhow::Context;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::MemoryRead;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use openhcl_tdisp::GuestToHostResponse;
use openhcl_tdisp::TdispResourceValidationInterface;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::hwid::HardwareIds;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use thiserror::Error;
use virt::IsolationType;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vpci_protocol as protocol;
use vpci_protocol::MAX_VPCI_TDISP_COMMAND_SIZE;
use vpci_protocol::SlotNumber;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unalign;

/// A VPCI client instance, for a single VPCI bus.
pub struct VpciClient {
    req: mesh::Sender<WorkerRequest>,
    task: Task<()>,
}

impl Inspect for VpciClient {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.req.send(WorkerRequest::Inspect(req.defer()))
    }
}

enum WorkerRequest {
    Inspect(inspect::Deferred),
    MapInterrupt(
        FailableRpc<
            (DeviceId, vpci_protocol::MsiResourceDescriptor2),
            protocol::MsiResourceRemapped,
        >,
    ),
    UnmapInterrupt(FailableRpc<(DeviceId, vpci_protocol::MsiResourceRemapped), ()>),
    QueryResourceRequirements(FailableRpc<DeviceId, protocol::QueryResourceRequirementsReply>),
    Init(FailableRpc<DeviceId, ()>),
    Done(DeviceId),
    TdispCommand(FailableRpc<protocol::VpciTdispCommand, GuestToHostResponse>),
}

#[derive(Debug, Copy, Clone, Inspect)]
struct DeviceId {
    #[inspect(hex, with = "|&x| u32::from(x)")]
    slot: SlotNumber,
    seq: u64,
}

#[derive(Inspect)]
struct VpciConnection<M: RingMem> {
    queue: Queue<M>,
}

impl<M: RingMem> VpciConnection<M> {
    async fn transact<
        S: IntoBytes + Immutable,
        R: FromBytes + IntoBytes + Immutable + KnownLayout,
    >(
        &mut self,
        send: S,
    ) -> anyhow::Result<R> {
        let (mut read, mut write) = self.queue.split();
        write
            .write(OutgoingPacket {
                transaction_id: 1,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[send.as_bytes()],
            })
            .await
            .context("failed to send protocol version query")?;

        let reply = read
            .read()
            .await
            .context("failed to read protocol version reply")?;
        let IncomingPacket::Completion(p) = &*reply else {
            anyhow::bail!("unexpected packet type")
        };
        let reply = p.reader().read_plain()?;
        Ok(reply)
    }

    async fn negotiate(&mut self) -> anyhow::Result<protocol::ProtocolVersion> {
        // Try to negotiate versions in order from newest to oldest. Hosts
        // that predate `RB` reply with `REVISION_MISMATCH`, so the
        // loop falls through to `VB`.
        let versions = &[protocol::ProtocolVersion::RB, protocol::ProtocolVersion::VB];

        for &version in versions {
            tracing::debug!(?version, "trying protocol version");

            // Create the protocol version query message
            let query = protocol::QueryProtocolVersion {
                message_type: protocol::MessageType::QUERY_PROTOCOL_VERSION,
                protocol_version: version,
            };

            let reply = self
                .transact::<_, protocol::QueryProtocolVersionReply>(query)
                .await
                .context("failed to send protocol version query")?;
            if reply.status == protocol::Status::SUCCESS {
                tracing::debug!(?version, "negotiated protocol version");
                return Ok(version);
            }
        }

        anyhow::bail!("no supported VPCI protocol version found");
    }
}

async fn send_eject_complete<M: RingMem>(
    write: &mut vmbus_async::queue::WriteHalf<'_, M>,
    slot: SlotNumber,
) -> anyhow::Result<()> {
    write
        .write(OutgoingPacket {
            transaction_id: 0,
            packet_type: vmbus_ring::OutgoingPacketType::InBandNoCompletion,
            payload: &[protocol::PdoMessage {
                message_type: protocol::MessageType::EJECT_COMPLETE,
                slot,
            }
            .as_bytes()],
        })
        .await?;

    Ok(())
}

/// Trait used to access configuration space of a VPCI bus.
pub trait MemoryAccess: Send {
    /// Returns the base GPA of the allocated MMIO space.
    fn gpa(&mut self) -> u64;
    /// Reads a 1-, 2-, or 4-byte value from the given address.
    fn read(&mut self, addr: u64, data: &mut [u8]);
    /// Writes a 1-, 2-, or 4-byte value to the given address.
    fn write(&mut self, addr: u64, data: &[u8]);
}

/// The amount of MMIO space required by the VPCI bus.
pub const MMIO_SIZE: u64 = 0x2000;

struct InspectableAsyncMutex<T>(futures::lock::Mutex<T>);

impl<T: Inspect> Inspect for InspectableAsyncMutex<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        match self.0.try_lock() {
            Some(guard) => guard.inspect(req),
            None => req.value("locked"),
        }
    }
}

/// A device description, which represents a VPCI device available on a bus.
#[derive(Inspect)]
pub struct VpciDeviceDescription {
    hw_ids: HardwareIds,
    #[inspect(skip)]
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    id: DeviceId,
    numa_node: u16,
    #[inspect(hex)]
    serial_num: u32,
    #[inspect(skip)]
    req: mesh::Sender<WorkerRequest>,
    #[inspect(skip)]
    eject: mesh::Receiver<VpciDeviceEjected>,
}

/// An initialized VPCI device.
#[derive(Inspect)]
pub struct VpciDevice {
    hw_ids: HardwareIds,
    #[inspect(skip)]
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    numa_node: u16,
    #[inspect(hex)]
    serial_num: u32,
    #[inspect(flatten)]
    dev: InUseDevice,
    shadows: Mutex<ConfigSpaceShadows>,
    #[inspect(hex, iter_by_index)]
    bar_masks: [u32; 6],
    #[inspect(hex, iter_by_index)]
    /// RAO == Read As One
    bar_rao: [u32; 6],
    tdisp: InspectableAsyncMutex<VpciClientTdispState>,
}

#[derive(Inspect)]
struct ConfigSpaceAccessor {
    #[inspect(skip)]
    mem: Box<dyn MemoryAccess>,
    #[inspect(hex)]
    base_gpa: u64,
    #[inspect(hex, with = "|&x| u32::from(x)")]
    current_slot: SlotNumber,
    #[inspect(iter_by_index)]
    slot_seq: Vec<u64>,
}

#[derive(Inspect)]
struct ConfigSpaceShadows {
    command: Command,
    #[inspect(hex, iter_by_index)]
    bars: [u32; 6],
}

impl ConfigSpaceAccessor {
    fn enable_slot(&mut self, id: DeviceId) {
        let i = u32::from(id.slot) as usize;
        if i >= self.slot_seq.len() {
            self.slot_seq.resize(i + 1, 0);
        }
        self.slot_seq[i] = id.seq;
    }

    fn disable_slot(&mut self, slot: SlotNumber) {
        let i = u32::from(slot) as usize;
        if let Some(s) = self.slot_seq.get_mut(i) {
            *s = 0;
        }
    }

    #[must_use]
    fn set_slot(&mut self, id: DeviceId) -> bool {
        if self
            .slot_seq
            .get(u32::from(id.slot) as usize)
            .is_none_or(|s| s != &id.seq)
        {
            return false;
        }
        if id.slot != self.current_slot {
            self.mem.write(
                self.base_gpa + protocol::MMIO_PAGE_SLOT_NUMBER,
                &u32::from(id.slot).to_ne_bytes(),
            );
            self.current_slot = id.slot;
        }
        true
    }

    /// Reads a value from the configuration space of the given device.
    /// Offset must be u32 aligned.
    fn read(&mut self, id: DeviceId, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        if !self.set_slot(id) {
            tracelimit::warn_ratelimited!(?id, offset, "device is gone, ignoring cfg read");
            value.set(!0);
            return;
        }
        let (byte_offset, _) = value.byte_enable().to_byte_offset_len();
        let addr = self.base_gpa
            + protocol::MMIO_PAGE_CONFIG_SPACE
            + (offset as u64)
            + (byte_offset as u64);
        self.mem
            .read(addr, value.reborrow().into_valid_byte_slice());
        tracing::trace!(?id, offset, ?value, "host config space read");
    }

    /// Writes a value to the configuration space of the given device.
    /// Offset must be u32 aligned.
    fn write(&mut self, id: DeviceId, offset: u16, value: ByteEnabledDwordWrite) {
        if !self.set_slot(id) {
            tracelimit::warn_ratelimited!(?id, offset, "device is gone, ignoring cfg write");
            return;
        }
        tracing::trace!(?id, offset, ?value, "host config space write");
        let (byte_offset, _) = value.byte_enable().to_byte_offset_len();
        let addr = self.base_gpa
            + protocol::MMIO_PAGE_CONFIG_SPACE
            + (offset as u64)
            + (byte_offset as u64);
        self.mem.write(addr, value.as_valid_byte_slice());
    }
}

#[derive(Inspect)]
struct InUseDevice {
    #[inspect(skip)]
    req: mesh::Sender<WorkerRequest>,
    id: DeviceId,
}

impl Drop for InUseDevice {
    fn drop(&mut self) {
        self.req.send(WorkerRequest::Done(self.id));
    }
}

impl VpciDeviceDescription {
    /// Returns the hardware IDs of the device.
    pub fn hw_ids(&self) -> &HardwareIds {
        &self.hw_ids
    }

    /// Returns the NUMA node of the device.
    pub fn numa_node(&self) -> u16 {
        self.numa_node
    }

    /// Returns the serial number of the device.
    pub fn serial_num(&self) -> u32 {
        self.serial_num
    }

    /// Initializes the device, returning a VPCI device instance that can be
    /// used to interact with it. Also returns an object to use to get notified
    /// when the device is ejected or surprise removed.
    pub async fn init(
        self,
        resource_validator: Arc<dyn TdispResourceValidationInterface>,
        isolation_type: IsolationType,
        vtom: u64,
        target_vtl: hvdef::Vtl,
    ) -> anyhow::Result<(VpciDevice, VpciDeviceEject)> {
        let requirements = self
            .req
            .call_failable(WorkerRequest::QueryResourceRequirements, self.id)
            .await?;

        tracing::debug!(
            bars = format_args!("{:#x?}", requirements.bars),
            "queried requirements"
        );

        let Self {
            hw_ids,
            config_space,
            id,
            numa_node,
            serial_num,
            req,
            eject,
        } = self;

        let tdisp = VpciClientTdispState::new(
            req.clone(),
            id.slot.into_bits() as u64,
            resource_validator,
            isolation_type,
            vtom,
            target_vtl,
        );

        // After this, the device is considered initialized and the caller is
        // responsible notifying the worker when the device is no longer in use.
        let dev = InUseDevice { req, id };

        dev.req.call_failable(WorkerRequest::Init, id).await?;

        let mut high64 = false;
        let mut bar_rao = [0; 6];
        for ((i, &bar), rao) in requirements.bars.iter().enumerate().zip(&mut bar_rao) {
            if high64 {
                high64 = false;
                *rao = 0;
            } else {
                let bits = pci_core::spec::cfg_space::BarEncodingBits::from(bar);
                if bits.use_pio() {
                    anyhow::bail!("BAR {} is PIO, which is not supported by VPCI", i);
                }
                *rao = bar & 0xf;
                high64 = bits.type_64_bit();
            }
        }

        let device = VpciDevice {
            shadows: Mutex::new(ConfigSpaceShadows {
                command: Command::new(),
                bars: [0; 6],
            }),
            bar_masks: requirements.bars,
            bar_rao,
            hw_ids,
            config_space,
            numa_node,
            serial_num,
            dev,
            tdisp: InspectableAsyncMutex(futures::lock::Mutex::new(tdisp)),
        };

        Ok((device, VpciDeviceEject(eject)))
    }
}

/// Stream that notifies that the device has been ejected or removed.
pub struct VpciDeviceEject(mesh::Receiver<VpciDeviceEjected>);

/// The kind of device removal.
pub enum RemovalKind {
    /// The host requested that the device be ejected.
    Eject,
    /// The host surprise removed the device.
    SurpriseRemove,
}

/// Notification that the device is being ejected.
///
/// The [`VpciDeviceEject`] stream will be closed when the device is actually
/// removed.
pub struct VpciDeviceEjected;

impl Stream for VpciDeviceEject {
    type Item = VpciDeviceEjected;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_next_unpin(cx)
    }
}

impl VpciDevice {
    /// Reads device configuration space.
    ///
    /// Some values will be handled without communicating with the host.
    /// Offset must be u32 aligned.
    pub fn read_cfg(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        // For static values, return values from the device's description.
        match HeaderType00(offset) {
            HeaderType00::STATUS_COMMAND => {
                let shadows = self.shadows.lock();
                self.config_space
                    .lock()
                    .read(self.dev.id, offset, value.reborrow());
                // Preserve the MMIO enabled bit in the command register, since
                // Hyper-V does not always emulate it correctly for reads.
                let mask = u32::from(u16::from(Command::new().with_mmio_enabled(true)));
                if value.valid_mask() & mask != 0 {
                    value.set(
                        (value.extract() & !mask) | (u32::from(u16::from(shadows.command)) & mask),
                    );
                }
            }
            HeaderType00::DEVICE_VENDOR => {
                value.set_low_high(self.hw_ids.vendor_id, self.hw_ids.device_id);
            }
            HeaderType00::CLASS_REVISION => {
                value.set_bytes(
                    self.hw_ids.revision_id,
                    self.hw_ids.prog_if.0,
                    self.hw_ids.sub_class.0,
                    self.hw_ids.base_class.0,
                );
            }
            HeaderType00::SUBSYSTEM_ID => {
                value.set_low_high(
                    self.hw_ids.type0_sub_vendor_id,
                    self.hw_ids.type0_sub_system_id,
                );
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                // The Hyper-V VPCI implementation does not consistently handle
                // BAR reads. Return the shadowed value.
                let shadows = self.shadows.lock();
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                value.set(shadows.bars[i] | self.bar_rao[i]);
            }
            _ => self
                .config_space
                .lock()
                .read(self.dev.id, offset, value.reborrow()),
        };
        tracing::trace!(?offset, ?value, "config space read at {:#x}", offset);
    }

    /// Writes device configuration space.
    /// Offset must be u32 aligned.
    pub fn write_cfg(&self, offset: u16, value: ByteEnabledDwordWrite) {
        tracing::trace!(?offset, ?value, "config space write at {:#x}", offset);
        let mut shadows = self.shadows.lock();
        let shadows = &mut *shadows;
        let mut accessor = self.config_space.lock();
        match HeaderType00(offset) {
            HeaderType00::STATUS_COMMAND => {
                let new_command = Command::from(value.merge_low(shadows.command.into_bits()));
                if new_command.mmio_enabled() && !shadows.command.mmio_enabled() {
                    // Flush the BAR shadow to the device.
                    for (i, &bar) in shadows.bars.iter().enumerate() {
                        let bar_offset = HeaderType00::BAR0.0 + (i as u16 * 4);
                        let write_dword = ByteEnabledDwordWrite::with_all_bytes_enabled(bar);
                        accessor.write(self.dev.id, bar_offset, write_dword);
                    }
                }
                shadows.command = new_command;
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                // Write the BAR shadow. Defer writing to the device until MMIO
                // is enabled to avoid wasting time writing probe values to the
                // host.
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                let new_bar_value = value.merge(shadows.bars[i]);
                shadows.bars[i] = new_bar_value & self.bar_masks[i] | self.bar_rao[i];
                return;
            }
            _ => {}
        }
        accessor.write(self.dev.id, offset, value);
    }

    /// Clear the MMIO-enable and bus-master bits in both the shadowed
    /// command register and on the host-side device, as if the guest
    /// had written a STATUS_COMMAND value with MMIO and bus-master
    /// disabled. Called on the unbind path
    /// ([`Self::tdisp_on_device_deactivate`]) to explicitly leave the command
    /// register in the expected off state after the device is unbound.
    ///
    /// This is not safety critical, this is for cleanup purposes only.
    ///
    /// TEMPORARY: neutered so that no path can upstream a command register
    /// write that turns MMIO off, including the attestation failure path in
    /// [`Self::tdisp_fail_attestation`]. Both the shadow and the host-side
    /// device are left untouched so the guest and the host keep agreeing that
    /// MMIO is on; the caller just logs and continues.
    fn clear_command_register(&self) {
        tracing::warn!(
            "clear_command_register: NOT clearing command register MMIO and bus-master bits; \
             upstreaming a command register write that turns MMIO off is temporarily disabled"
        );
    }

    /// Called on the STATUS_COMMAND MMIO disabled→enabled edge.
    ///
    /// If the TDI is not already in `Run`, this will drive a bind/attest cycle
    /// first. If the TDI is already in `Run`, this will unbind and rebind the
    /// TDI to attest the device again.
    ///
    /// Then writes `command_value` to the command register, which flushes the
    /// shadowed BARs to the host device, and only after that notifies TDISP
    /// about each currently active MMIO BAR via
    /// [`tdisp::VpciClientTdispState::tdisp_on_mmio_reconfigured`]. The BARs
    /// must be mapped for the guest before the unblock operations run.
    ///
    /// Returns `true` only if attestation and every BAR notification succeeded
    /// completely. On `false` the caller must not treat the device as
    /// activated, though the command register may still be on: see
    /// [`Self::clear_command_register`].
    pub async fn tdisp_on_device_activate(&self, command_value: ByteEnabledDwordWrite) -> bool {
        use tdisp::TdispVpciAttestationInterface;

        tracing::info!(
            "tdisp_on_device_activate: guest enabled MMIO, attesting device and notifying TDISP of MMIO bars"
        );

        // Attest the device.
        let attest_result = match self.tdisp_query_capabilities().await {
            Ok(interface_info) => self
                .tdisp_attest_device(interface_info)
                .await
                .context("tdisp_attest_device failed"),
            Err(err) => Err(err.context("tdisp_query_capabilities failed")),
        };

        if let Err(err) = attest_result {
            tracing::error!(
                error = &*err as &dyn std::error::Error,
                "tdisp_on_device_activate: attestation failed"
            );
            self.tdisp_fail_attestation().await;
            return false;
        }

        // Attestation succeeded, so enable the command register now. This
        // flushes the shadowed BARs to the host device, mapping the MMIO
        // ranges for the guest before the unblock operations below run.
        //
        // On any failure past this point `tdisp_fail_attestation` clears the
        // command register again.
        self.write_cfg(HeaderType00::STATUS_COMMAND.0, command_value);

        tracing::info!(
            ?command_value,
            "tdisp_on_device_activate: command register written at {:#x}, MMIO BARs are now mapped",
            HeaderType00::STATUS_COMMAND.0,
        );

        let bars = self.shadows.lock().bars;

        tracing::debug!(?bars, ?self.bar_masks, "command register write enabled mmio, notifying TDISP of MMIO bars");

        let mut i = 0usize;
        while i < bars.len() {
            let mask = self.bar_masks[i];
            if mask == 0 {
                i += 1;
                continue;
            }

            let bits = pci_core::spec::cfg_space::BarEncodingBits::from(mask);

            // Decode the BAR values to determine what the base address and length of the MMIO ranges configured by the guest.
            let (bar_id, base_address, length_bytes, next_i) = if bits.type_64_bit() && i + 1 < 6 {
                // Combine both 32-bit masks and bases into 64-bit values. Mask off low 4 bits used for flags.
                let base = ((bars[i + 1] as u64) << 32) | ((bars[i] & !0xF_u32) as u64);
                let full_mask = ((self.bar_masks[i + 1] as u64) << 32) | ((mask & !0xF_u32) as u64);
                let size = (!full_mask).wrapping_add(1);
                let size_u32 = u32::try_from(size).unwrap_or_else(|_| {
                    tracing::warn!(bar_id = i, size, "64-bit BAR size exceeds u32");
                    0
                });
                (i as u16, base, size_u32, i + 2)
            } else {
                let base = (bars[i] & !0xF_u32) as u64;
                let size = (!(mask & !0xF_u32)).wrapping_add(1);
                (i as u16, base, size, i + 1)
            };

            tracing::debug!(
                ?self.bar_masks,
                ?bars,
                bar_id,
                base_address,
                length_bytes,
                "tdisp_on_device_activate"
            );

            if base_address != 0 && length_bytes != 0 {
                tracing::info!(
                    bar_id,
                    base_address,
                    length_bytes,
                    "notifying TDISP state of active MMIO BAR"
                );
                if let Err(e) = self
                    .tdisp_on_mmio_reconfigured(bar_id, base_address, length_bytes)
                    .await
                {
                    tracing::error!(
                        bar_id,
                        base_address,
                        length_bytes,
                        error = %e,
                        "failed to notify TDISP of active MMIO BAR. Failing activation."
                    );
                    self.tdisp_fail_attestation().await;
                    return false;
                }
            }

            i = next_i;
        }

        tracing::info!(
            "tdisp_on_device_activate: attestation and MMIO unblock complete, device activated"
        );

        true
    }

    /// Common teardown for any failure during the MMIO-enable activation
    /// path: post-Bind attestation failure, per-BAR unblock failure, or
    /// similar. Unbinds the device, clears the command register, and cleans up
    /// resources.
    async fn tdisp_fail_attestation(&self) {
        use openhcl_tdisp::TdispGuestUnbindReason;
        use openhcl_tdisp::TdispVirtualDeviceInterface;

        tracing::error!(
            "tdisp_fail_attestation: unbinding TDI back to Unlocked due to attestation failure"
        );

        let unbind_result = self
            .tdisp_unbind(TdispGuestUnbindReason::ResourceSetupFailure)
            .await;

        if let Err(unbind_err) = &unbind_result {
            tracing::warn!(
                error = unbind_err.as_ref() as &dyn std::error::Error,
                "tdisp_fail_attestation: unbind failed"
            );
        }

        // Leave the command register alone so the device is not walked back to
        // the off state after a failed activation. See
        // `clear_command_register`.
        self.clear_command_register();
    }

    /// Notifies TDISP that the guest has disabled MMIO on this device. If the
    /// TDI is in `Run`, issues a full `tdisp_unbind` so the TDI returns to
    /// `Unlocked` and *all* per-attest state (cached interface report, device
    /// id, intercepted BARs, validated MMIO bars, DMA flag) is cleared. Mirrors
    /// [`Self::tdisp_on_device_activate`] for the disable edge.
    ///
    /// Called when STATUS_COMMAND transitions MMIO from enabled to disabled. If
    /// the TDI is not in `Run` (e.g. `Uninitialized`, `Unlocked`, or `Locked`),
    /// this is a no-op.
    pub async fn tdisp_on_device_deactivate(&self) {
        use openhcl_tdisp::TdispGuestUnbindReason;
        use openhcl_tdisp::TdispVirtualDeviceInterface;
        use tdisp::TdispVpciAttestationInterface;

        let state = self.tdisp_tdi_state().await;

        tracing::info!(
            ?state,
            "tdisp_on_device_deactivate: guest disabled MMIO, unbinding TDI back to Unlocked"
        );

        if let Err(err) = self.tdisp_unbind(TdispGuestUnbindReason::Graceful).await {
            tracing::warn!(
                error = &*err as &dyn std::error::Error,
                "tdisp_on_device_deactivate: unbind failed"
            );
        }

        // Always clear the command register explicitly on the unbind path so the
        // device is left in the expected off state regardless of the value the
        // guest wrote or the outcome of the unbind.
        self.clear_command_register();
    }
}

#[derive(Error, Debug)]
#[error("invalid vector count: {0}")]
struct InvalidVectorCount(u32);

#[derive(Error, Debug)]
#[error("starting vector too large: {0}")]
struct VectorTooLarge(u32);

#[derive(Error, Debug)]
#[error("invalid processor number: {0}")]
struct InvalidProcessor(u32);

impl MapVpciInterrupt for VpciDevice {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &vmcore::vpci_msi::VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        let mut interrupt = protocol::MsiResourceDescriptor2 {
            // TODO: use MsiResourceDescriptor3 to support ARM64.
            vector: params
                .vector
                .try_into()
                .map_err(|_| RegisterInterruptError::new(VectorTooLarge(params.vector)))?,
            delivery_mode: if params.multicast {
                protocol::DeliveryMode::LOWEST_PRIORITY
            } else {
                protocol::DeliveryMode::FIXED
            },
            vector_count: vector_count
                .try_into()
                .map_err(|_| RegisterInterruptError::new(InvalidVectorCount(vector_count)))?,
            processor_count: 0,
            processor_array: [0; 32],
            reserved: 0,
        };
        for (d, &s) in interrupt
            .processor_array
            .iter_mut()
            .zip(params.target_processors)
        {
            *d = s
                .try_into()
                .map_err(|_| RegisterInterruptError::new(InvalidProcessor(s)))?;
            interrupt.processor_count += 1;
        }
        let resource = self
            .dev
            .req
            .call_failable(WorkerRequest::MapInterrupt, (self.dev.id, interrupt))
            .await
            .map_err(RegisterInterruptError::new)?;

        tracing::debug!(
            address = resource.address,
            data = resource.data_payload,
            "registered interrupt"
        );

        Ok(MsiAddressData {
            address: resource.address,
            data: resource.data_payload,
        })
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        tracing::debug!(address, data, "unregistering interrupt");
        let interrupt = protocol::MsiResourceRemapped {
            reserved: 0,
            message_count: 0, // The host does not look at this value, so don't bother to remember it.
            data_payload: data,
            address,
        };
        self.dev
            .req
            .call_failable(WorkerRequest::UnmapInterrupt, (self.dev.id, interrupt))
            .await
            .unwrap_or_else(|err| {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to unregister interrupt"
                );
            });
    }
}

#[derive(InspectMut)]
struct VpciClientWorker<M: RingMem> {
    conn: VpciConnection<M>,
    #[inspect(flatten)]
    state: WorkerState,
}

#[derive(Inspect)]
struct WorkerState {
    #[inspect(iter_by_key)]
    tx: slab::Slab<Tx>,
    #[inspect(skip)]
    req: mesh::Receiver<WorkerRequest>,
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    #[inspect(debug)]
    protocol_version: protocol::ProtocolVersion,
    #[inspect(skip)]
    send_devices: mesh::Sender<VpciDeviceDescription>,
    #[inspect(skip)]
    init_devices: Option<Vec<VpciDeviceDescription>>,
    #[inspect(iter_by_index)]
    slots: Vec<Option<SlotState>>,
    next_seq: u64,
    #[inspect(skip)]
    buf: Vec<u8>,
}

#[derive(Inspect)]
struct SlotState {
    hw_ids: HardwareIds,
    serial_num: u32,
    in_use: bool,
    removed: bool,
    ejected: bool,
    #[inspect(skip)]
    eject: mesh::Sender<VpciDeviceEjected>,
    seq: u64,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum Tx {
    FdoD0Entry(
        #[inspect(skip)] mesh::OneshotSender<Result<Vec<VpciDeviceDescription>, protocol::Status>>,
    ),
    CreateInterrupt(#[inspect(skip)] FailableRpc<(), protocol::MsiResourceRemapped>),
    DeleteInterrupt(#[inspect(skip)] FailableRpc<(), ()>),
    QueryResourceRequirements(
        #[inspect(skip)] FailableRpc<(), protocol::QueryResourceRequirementsReply>,
    ),
    AssignedResources(#[inspect(skip)] FailableRpc<(), ()>),
    TdispCommand(#[inspect(skip)] FailableRpc<(), GuestToHostResponse>),
}

impl VpciClient {
    /// Instantiates a new VPCI client, connecting to the VPCI bus avilable via
    /// `channel`. Returns the initial set of devices available on the bus.
    ///
    /// `mmio` is used to access the two pages of MMIO space used for
    /// configuration space. `devices` will receive dynamically added devices as
    /// they are added to the bus.
    pub async fn connect<M: 'static + RingMem + Sync>(
        driver: impl Spawn,
        channel: RawAsyncChannel<M>,
        mut mmio: Box<dyn MemoryAccess>,
        devices: mesh::Sender<VpciDeviceDescription>,
    ) -> anyhow::Result<(Self, Vec<VpciDeviceDescription>)> {
        let mut conn = VpciConnection {
            queue: Queue::new(channel)?,
        };

        let version = conn
            .negotiate()
            .await
            .context("failed to negotiate protocol version")?;

        let gpa = mmio.gpa();

        tracing::debug!(gpa, "requesting fdo d0 entry");

        let mut tx = slab::Slab::new();

        // Start a transaction to move the bus to the D0 state. The completion
        // may come after the device list, so start the task and wait for the
        // reply afterwards.
        let (fdo_entry_send, fdo_entry_recv) = mesh::oneshot();
        let tx_id = index_to_tx_id(tx.insert(Tx::FdoD0Entry(fdo_entry_send)));
        conn.queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: tx_id,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[protocol::FdoD0Entry {
                    message_type: protocol::MessageType::FDO_D0_ENTRY,
                    padding: 0,
                    mmio_start: gpa,
                }
                .as_bytes()],
            })
            .await
            .context("failed to send FDO D0 entry")?;

        let (req_send, req_recv) = mesh::channel();
        let worker = VpciClientWorker {
            conn,
            state: WorkerState {
                tx,
                req: req_recv,
                protocol_version: version,
                send_devices: devices,
                config_space: Arc::new(Mutex::new(ConfigSpaceAccessor {
                    mem: mmio,
                    base_gpa: gpa,
                    // Let's not assume the config space access starts at slot 0.
                    current_slot: (!0).into(),
                    slot_seq: Vec::new(),
                })),
                init_devices: Some(Vec::new()),
                slots: Vec::new(),
                next_seq: 1,
                buf: vec![0; protocol::MAXIMUM_PACKET_SIZE],
            },
        };

        let task = driver.spawn("vpci-client", worker.run());
        let r = fdo_entry_recv
            .await
            .context("no response to FDO D0 entry")?;

        let init_devices = match r {
            Ok(v) => v,
            Err(status) => {
                task.cancel().await;
                anyhow::bail!("failed to enter D0 state: {:#x?}", status);
            }
        };

        tracing::debug!(gpa, "fdo d0 entry successful");

        let this = Self {
            req: req_send,
            task,
        };

        Ok((this, init_devices))
    }

    /// Shuts down the VPCI bus client.
    pub async fn shutdown(self) {
        drop(self.req);
        self.task.await;
    }

    /// Detaches the task from the client, allowing it to run independently.
    pub fn detach(self) {
        self.task.detach();
    }
}

impl<M: RingMem> VpciClientWorker<M> {
    async fn run(mut self) {
        if let Err(err) = self.run_inner().await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "vpci client worker failed"
            );
        }
    }

    async fn run_inner(&mut self) -> anyhow::Result<()> {
        loop {
            let (mut read, mut write) = self.conn.queue.split();
            let deferred = {
                enum Event<T, U> {
                    Packet(T),
                    Request(U),
                }

                let read_packet = read.read().map(Event::Packet);
                let req = self.state.req.next().map(Event::Request);

                let event = (read_packet, req).race().await;
                match event {
                    Event::Packet(p) => {
                        let p = p.context("failed to read packet")?;
                        match &*p {
                            IncomingPacket::Data(p) => {
                                self.state.handle_packet(&mut write, p).await?;
                            }
                            IncomingPacket::Completion(p) => {
                                self.state.handle_completion(p)?;
                            }
                        }
                        None
                    }
                    Event::Request(Some(req)) => self.state.handle_req(&mut write, req).await?,
                    Event::Request(None) => break,
                }
            };
            if let Some(deferred) = deferred {
                deferred.inspect(&mut *self);
            }
        }
        Ok(())
    }
}

impl WorkerState {
    fn slot_mut(&mut self, id: DeviceId) -> Option<&mut SlotState> {
        let slot_index = u32::from(id.slot) as usize;
        let slot = self.slots.get_mut(slot_index)?.as_mut()?;
        if slot.seq != id.seq {
            return None;
        }
        assert!(!slot.removed);
        Some(slot)
    }

    async fn handle_packet<M: RingMem>(
        &mut self,
        write: &mut vmbus_async::queue::WriteHalf<'_, M>,
        p: &vmbus_async::queue::DataPacket<'_, M>,
    ) -> anyhow::Result<()> {
        let mut reader = p.reader();
        let len = reader.len();
        let buf = self.buf.get_mut(..len).context("packet too large")?;
        reader.read(buf)?;

        let (packet_type, _) = protocol::MessageType::read_from_prefix(buf)
            .ok()
            .context("packet too small")?;

        tracing::debug!(?packet_type, "received packet");

        match packet_type {
            protocol::MessageType::BUS_RELATIONS2 => {
                let (bus_relations, devices) = protocol::QueryBusRelations2::read_from_prefix(buf)
                    .ok()
                    .context("failed to read bus relations")?;

                let (devices, _) =
                    <[Unalign<protocol::DeviceDescription2>]>::ref_from_prefix_with_elems(
                        devices,
                        bus_relations.device_count as usize,
                    )
                    .ok()
                    .context("failed to read bus relation devices")?;

                for slot in self.slots.iter_mut().flatten() {
                    slot.removed = true;
                }

                for device in devices {
                    let device = device.get();
                    let slot_index = u32::from(device.slot) as usize;
                    if slot_index >= u8::MAX as usize {
                        anyhow::bail!("invalid slot index {slot_index}");
                    }
                    if let Some(Some(slot)) = self.slots.get_mut(slot_index) {
                        if slot.hw_ids.device_id == device.pnp_id.device_id
                            && slot.hw_ids.vendor_id == device.pnp_id.vendor_id
                            && slot.serial_num == device.serial_num
                        {
                            slot.removed = false;
                            continue;
                        }
                        self.slots[slot_index] = None;
                    }

                    let hw_ids = HardwareIds {
                        vendor_id: device.pnp_id.vendor_id,
                        device_id: device.pnp_id.device_id,
                        revision_id: device.pnp_id.revision_id,
                        prog_if: device.pnp_id.prog_if.into(),
                        sub_class: device.pnp_id.sub_class.into(),
                        base_class: device.pnp_id.base_class.into(),
                        type0_sub_vendor_id: device.pnp_id.sub_vendor_id,
                        type0_sub_system_id: device.pnp_id.sub_system_id,
                    };

                    if slot_index >= self.slots.len() {
                        self.slots.resize_with(slot_index + 1, || None);
                    }
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    let (eject_send, eject_recv) = mesh::channel();
                    self.slots[slot_index] = Some(SlotState {
                        hw_ids,
                        serial_num: device.serial_num,
                        removed: false,
                        ejected: false,
                        eject: eject_send,
                        in_use: false,
                        seq,
                    });
                    let vpci_device = VpciDeviceDescription {
                        hw_ids,
                        config_space: self.config_space.clone(),
                        id: DeviceId {
                            slot: device.slot,
                            seq,
                        },
                        numa_node: device.numa_node,
                        serial_num: device.serial_num,
                        req: self.req.sender(),
                        eject: eject_recv,
                    };
                    if let Some(init_devices) = &mut self.init_devices {
                        init_devices.push(vpci_device);
                    } else {
                        self.send_devices.send(vpci_device);
                    }
                }

                for (slot_index, slot_slot) in self.slots.iter_mut().enumerate() {
                    let Some(slot) = slot_slot else { continue };
                    if !slot.removed {
                        continue;
                    }
                    self.config_space
                        .lock()
                        .disable_slot((slot_index as u32).into());
                    *slot_slot = None;
                }
            }
            protocol::MessageType::EJECT => {
                let (eject, _) = protocol::PdoMessage::read_from_prefix(buf)
                    .ok()
                    .context("failed to read eject packet")?;
                let slot_index = u32::from(eject.slot) as usize;
                let Some(Some(slot)) = self.slots.get_mut(slot_index) else {
                    anyhow::bail!("eject packet for unknown slot {slot_index}");
                };
                if !std::mem::replace(&mut slot.ejected, true) {
                    if slot.in_use {
                        slot.eject.send(VpciDeviceEjected);
                    } else {
                        send_eject_complete(write, eject.slot).await?;
                    }
                } else {
                    tracing::warn!("eject packet for device that is already ejected");
                }
            }
            p => {
                anyhow::bail!("unexpected packet type: {:?}", p);
            }
        }
        Ok(())
    }

    fn handle_completion<M: RingMem>(
        &mut self,
        p: &vmbus_async::queue::CompletionPacket<'_, M>,
    ) -> Result<(), anyhow::Error> {
        let tx_id = p.transaction_id();
        let entry = self
            .tx
            .try_remove(tx_id_to_index(tx_id))
            .context("failed to find tx entry")?;
        let status = p
            .reader()
            .read_plain::<protocol::Status>()
            .context("failed to read tx reply")?;
        match entry {
            Tx::FdoD0Entry(send) => {
                tracing::trace!(tx_id, ?status, "fdo d0 entry reply received");
                let r = if status == protocol::Status::SUCCESS {
                    Ok(self.init_devices.take().unwrap())
                } else {
                    Err(status)
                };
                send.send(r);
            }
            Tx::CreateInterrupt(rpc) => {
                tracing::trace!(tx_id, ?status, "create interrupt reply received");

                if status == protocol::Status::SUCCESS {
                    let reply = p
                        .reader()
                        .read_plain::<protocol::CreateInterruptReply>()
                        .context("failed to read create interrupt reply")?;
                    rpc.complete(Ok(reply.interrupt));
                } else {
                    rpc.fail(anyhow::anyhow!("failed to create interrupt: {status:#x?}",));
                }
            }
            Tx::DeleteInterrupt(rpc) => {
                tracing::trace!(tx_id, "delete interrupt reply received");

                if status == protocol::Status::SUCCESS {
                    rpc.complete(Ok(()));
                } else {
                    rpc.fail(anyhow::anyhow!("failed to delete interrupt: {status:#x?}",));
                }
            }
            Tx::AssignedResources(rpc) => {
                tracing::trace!(tx_id, ?status, "assigned resources reply received");

                if status == protocol::Status::SUCCESS {
                    rpc.complete(Ok(()));
                } else {
                    rpc.fail(anyhow::anyhow!("failed to initialize device: {status:#x?}",));
                }
            }
            Tx::QueryResourceRequirements(rpc) => {
                tracing::trace!(tx_id, ?status, "query resource requirements reply received");

                if status == protocol::Status::SUCCESS {
                    let reply = p
                        .reader()
                        .read_plain::<protocol::QueryResourceRequirementsReply>()
                        .context("failed to read query resource requirements reply")?;
                    rpc.complete(Ok(reply));
                } else {
                    rpc.fail(anyhow::anyhow!(
                        "failed to query resource requirements: {status:#x?}",
                    ));
                }
            }
            Tx::TdispCommand(rpc) => {
                if status == protocol::Status::SUCCESS {
                    let mut reader = p.reader();

                    if reader.len() == 0 {
                        rpc.fail(anyhow::anyhow!("Unexpected empty response from host"));
                        return Ok(());
                    }

                    let header = reader
                        .read_plain::<protocol::VpciTdispCommandHeader>()
                        .context("failed to read tdisp command header")?;

                    let data_len = header.data_length as usize;
                    if data_len > MAX_VPCI_TDISP_COMMAND_SIZE {
                        rpc.fail(anyhow::anyhow!(
                            "Received TdispCommand data length exceeds maximum allowed: {} > {}",
                            data_len,
                            MAX_VPCI_TDISP_COMMAND_SIZE
                        ));
                        return Ok(());
                    }

                    // Allocate a mutable vector with the correct size
                    let mut data: Vec<u8> = vec![0; data_len];

                    // Read data_len bytes from start_of_data into Vec
                    reader
                        .read(data.as_mut_slice())
                        .context("failed to read tdisp command data")?;

                    let host_response = openhcl_tdisp::deserialize_response(data.as_slice())
                        .context("failed to deserialize tdisp response");

                    rpc.complete(host_response.map_err(mesh::error::RemoteError::new));
                } else {
                    if status == protocol::Status::NOT_SUPPORTED {
                        rpc.fail(anyhow::anyhow!(
                            "TDISP interface is not supported by this device or host"
                        ));
                    } else {
                        rpc.fail(anyhow::anyhow!(
                            "vmbus server responded error status: {status:#x?}",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_req<M: RingMem>(
        &mut self,
        write: &mut vmbus_async::queue::WriteHalf<'_, M>,
        req: WorkerRequest,
    ) -> anyhow::Result<Option<inspect::Deferred>> {
        match req {
            WorkerRequest::Inspect(deferred) => return Ok(Some(deferred)),
            WorkerRequest::MapInterrupt(rpc) => {
                let ((id, interrupt), reply) = rpc.split();
                if self.slot_mut(id).is_none() {
                    reply.fail(anyhow::anyhow!("device is gone"));
                    return Ok(None);
                }
                self.send_tx(
                    write,
                    Tx::CreateInterrupt(reply),
                    vpci_protocol::CreateInterrupt2 {
                        message_type: protocol::MessageType::CREATE_INTERRUPT2,
                        slot: id.slot,
                        interrupt,
                    },
                    &[],
                )
                .await
                .context("failed to send create interrupt message")?;
            }
            WorkerRequest::UnmapInterrupt(rpc) => {
                let ((id, interrupt), reply) = rpc.split();
                if self.slot_mut(id).is_none() {
                    reply.fail(anyhow::anyhow!("device is gone"));
                    return Ok(None);
                }
                self.send_tx(
                    write,
                    Tx::DeleteInterrupt(reply),
                    vpci_protocol::DeleteInterrupt {
                        message_type: protocol::MessageType::DELETE_INTERRUPT,
                        slot: id.slot,
                        interrupt,
                    },
                    &[],
                )
                .await
                .context("failed to send delete interrupt message")?;
            }
            WorkerRequest::Init(rpc) => {
                let (id, reply) = rpc.split();
                let Some(slot) = self.slot_mut(id) else {
                    reply.fail(anyhow::anyhow!("device is gone"));
                    return Ok(None);
                };
                slot.in_use = true;
                self.config_space.lock().enable_slot(id);
                // Send space for one resource to satisfy the Hyper-V implementation.
                self.send_tx(
                    write,
                    Tx::AssignedResources(reply),
                    protocol::DeviceTranslate {
                        message_type: protocol::MessageType::ASSIGNED_RESOURCES,
                        slot: id.slot,
                        ..FromZeros::new_zeroed()
                    },
                    &[0; size_of::<vpci_protocol::MsiResource3>()],
                )
                .await
                .context("failed to send assigned resources request")?;
            }
            WorkerRequest::QueryResourceRequirements(rpc) => {
                let (id, reply) = rpc.split();
                if self.slot_mut(id).is_none() {
                    reply.fail(anyhow::anyhow!("device is gone"));
                    return Ok(None);
                }
                self.send_tx(
                    write,
                    Tx::QueryResourceRequirements(reply),
                    protocol::QueryResourceRequirements {
                        message_type: protocol::MessageType::CURRENT_RESOURCE_REQUIREMENTS,
                        slot: id.slot,
                    },
                    &[],
                )
                .await
                .context("failed to send query resource requirements request")?;
            }
            WorkerRequest::Done(id) => {
                let Some(slot) = self.slot_mut(id) else {
                    return Ok(None);
                };
                slot.in_use = false;
                if slot.ejected {
                    send_eject_complete(write, id.slot).await?;
                }
            }
            WorkerRequest::TdispCommand(rpc) => {
                let (req, reply) = rpc.split();
                self.send_tx(
                    write,
                    Tx::TdispCommand(reply),
                    req.header,
                    req.data.as_slice(),
                )
                .await
                .context("failed to send tdisp command message")?;
            }
        }
        Ok(None)
    }

    async fn send_tx<S: IntoBytes + Immutable, M: RingMem>(
        &mut self,
        write: &mut vmbus_async::queue::WriteHalf<'_, M>,
        tx: Tx,
        msg: S,
        extra: &[u8],
    ) -> anyhow::Result<()> {
        let entry = self.tx.vacant_entry();
        let tx_id = index_to_tx_id(entry.key());
        tracing::trace!(
            tx_id,
            message = std::any::type_name_of_val(&msg),
            "sending transaction"
        );
        write
            .write(OutgoingPacket {
                transaction_id: tx_id,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[msg.as_bytes(), extra],
            })
            .await
            .context("failed to send transaction")?;

        entry.insert(tx);
        Ok(())
    }
}

fn index_to_tx_id(index: usize) -> u64 {
    // Hyper-V VPCI doesn't like transaction IDs of 0, so we start at 1.
    (index + 1) as u64
}

fn tx_id_to_index(tx_id: u64) -> usize {
    tx_id.saturating_sub(1) as usize
}
