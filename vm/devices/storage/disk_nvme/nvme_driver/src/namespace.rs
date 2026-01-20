// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe namespace frontend.

use super::spec;
use super::spec::nvm;
use crate::NVME_PAGE_SHIFT;
use crate::driver::IoIssuers;
use crate::driver::save_restore::SavedNamespaceData;
use crate::queue_pair::Issuer;
use crate::queue_pair::RequestError;
use crate::queue_pair::admin_cmd;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use vmcore::vm_task::VmTaskDriver;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// An error getting a namespace.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum NamespaceError {
    #[error("namespace not found")]
    NotFound,
    #[error("formatted lba size invalid")]
    FlbasInvalid,
    #[error("lba format invalid: {0:?}")]
    LbaFormatInvalid(nvm::Lbaf),
    #[error("nvme request failed")]
    Request(#[source] RequestError),
    #[error("maximum data transfer size too small: 2^{0} pages")]
    MdtsInvalid(u8),
    #[error("requesting a duplicate namespace: {0}")]
    Duplicate(u32),
}

/// A handle to an NVMe namespace that enforces single-ownership semantics.
/// By not exposing `Arc<Namespace>` directly, this wrapper prevents cloning,
/// ensuring the nvme driver can rely on exclusive access for Namespace management.
#[derive(Debug, Inspect)]
pub struct NamespaceHandle {
    namespace: Arc<Namespace>,
}

impl NamespaceHandle {
    /// Creates a new handle
    pub fn new(namespace: Arc<Namespace>) -> Self {
        Self { namespace }
    }

    /// Gets the current block count.
    pub fn block_count(&self) -> u64 {
        self.namespace.block_count()
    }

    /// Wait for the block count to be different from `block_count`.
    pub async fn wait_resize(&self, block_count: u64) -> u64 {
        self.namespace.wait_resize(block_count).await
    }

    /// Gets the block size in bytes.
    pub fn block_size(&self) -> u32 {
        self.namespace.block_size()
    }

    /// Reads from the namespace.
    pub async fn read(
        &self,
        target_cpu: u32,
        lba: u64,
        block_count: u32,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), RequestError> {
        self.namespace
            .read(target_cpu, lba, block_count, guest_memory, mem)
            .await
    }

    /// Writes to the namespace.
    pub async fn write(
        &self,
        target_cpu: u32,
        lba: u64,
        block_count: u32,
        fua: bool,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), RequestError> {
        self.namespace
            .write(target_cpu, lba, block_count, fua, guest_memory, mem)
            .await
    }

    /// Flushes the namespace to persistent media.
    pub async fn flush(&self, target_cpu: u32) -> Result<(), RequestError> {
        self.namespace.flush(target_cpu).await
    }

    /// Returns the maximum size for a read or write, in blocks.
    pub fn max_transfer_block_count(&self) -> u32 {
        self.namespace.max_transfer_block_count()
    }

    /// Returns whether the namespace support dataset management, needed to call
    /// [`Self::deallocate`].
    pub fn supports_dataset_management(&self) -> bool {
        self.namespace.supports_dataset_management()
    }

    /// The preferred granularity for unmap requests.
    pub fn preferred_deallocate_granularity(&self) -> u16 {
        self.namespace.preferred_deallocate_granularity()
    }

    /// Returns the maximum number of ranges to pass to [`Self::deallocate`].
    pub fn dataset_management_range_limit(&self) -> usize {
        self.namespace.dataset_management_range_limit()
    }

    /// Returns the maximum size of a single range to pass to
    /// [`Self::deallocate`].
    pub fn dataset_management_range_size_limit(&self) -> u32 {
        self.namespace.dataset_management_range_size_limit()
    }

    /// Issues a dataset management command to deallocate the specified ranges.
    ///
    /// The device may ignore ranges or LBA counts beyond a certain point. Use
    /// [`Self::dataset_management_range_limit`] and
    /// [`Self::dataset_management_range_size_limit`] to get the
    /// controller-reported bounds.
    pub async fn deallocate(
        &self,
        target_cpu: u32,
        ranges: &[nvm::DsmRange],
    ) -> Result<(), RequestError> {
        self.namespace.deallocate(target_cpu, ranges).await
    }

    /// Gets the namespace's reservation capabilities.
    pub fn reservation_capabilities(&self) -> nvm::ReservationCapabilities {
        self.namespace.reservation_capabilities()
    }

    /// Gets the namespace's reservation report.
    pub async fn reservation_report_extended(
        &self,
        target_cpu: u32,
    ) -> Result<
        (
            nvm::ReservationReportExtended,
            Vec<nvm::RegisteredControllerExtended>,
        ),
        RequestError,
    > {
        self.namespace.reservation_report_extended(target_cpu).await
    }

    /// Acquires a reservation.
    pub async fn reservation_acquire(
        &self,
        target_cpu: u32,
        action: nvm::ReservationAcquireAction,
        crkey: u64,
        prkey: u64,
        reservation_type: nvm::ReservationType,
    ) -> Result<(), RequestError> {
        self.namespace
            .reservation_acquire(target_cpu, action, crkey, prkey, reservation_type)
            .await
    }

    /// Releases a reservation.
    pub async fn reservation_release(
        &self,
        target_cpu: u32,
        action: nvm::ReservationReleaseAction,
        crkey: u64,
        reservation_type: nvm::ReservationType,
    ) -> Result<(), RequestError> {
        self.namespace
            .reservation_release(target_cpu, action, crkey, reservation_type)
            .await
    }

    /// Modifies a reservation registration.
    pub async fn reservation_register(
        &self,
        target_cpu: u32,
        action: nvm::ReservationRegisterAction,
        crkey: Option<u64>,
        nrkey: u64,
        ptpl: Option<bool>,
    ) -> Result<(), RequestError> {
        self.namespace
            .reservation_register(target_cpu, action, crkey, nrkey, ptpl)
            .await
    }

    /// Return Namespace ID.
    pub fn nsid(&self) -> u32 {
        self.namespace.nsid()
    }
}

/// An NVMe namespace.
#[derive(Debug, Inspect)]
pub struct Namespace {
    nsid: u32,
    #[inspect(flatten)]
    state: Arc<DynamicState>,
    block_shift: u32,
    max_transfer_block_count: u32,
    preferred_deallocate_granularity: u16,
    reservation_capabilities: nvm::ReservationCapabilities,
    controller_identify: Arc<spec::IdentifyController>,
    #[inspect(skip)]
    issuers: Arc<IoIssuers>,
}

#[derive(Debug, Inspect)]
struct DynamicState {
    block_count: AtomicU64,
    #[inspect(skip)]
    resize_event: event_listener::Event,
    removed: AtomicBool,
    identify: Mutex<nvm::IdentifyNamespace>,
}

impl Namespace {
    pub(super) async fn new(
        driver: &VmTaskDriver,
        admin: Arc<Issuer>,
        rescan_event: mesh::Receiver<()>,
        controller_identify: Arc<spec::IdentifyController>,
        io_issuers: &Arc<IoIssuers>,
        nsid: u32,
    ) -> Result<Self, NamespaceError> {
        let identify = identify_namespace(&admin, nsid)
            .await
            .map_err(NamespaceError::Request)?;

        Namespace::new_from_identify(
            driver,
            admin,
            rescan_event,
            controller_identify.clone(),
            io_issuers,
            nsid,
            identify,
        )
    }

    /// Create Namespace object from Identify data structure.
    fn new_from_identify(
        driver: &VmTaskDriver,
        admin: Arc<Issuer>,
        rescan_event: mesh::Receiver<()>,
        controller_identify: Arc<spec::IdentifyController>,
        io_issuers: &Arc<IoIssuers>,
        nsid: u32,
        identify: nvm::IdentifyNamespace,
    ) -> Result<Self, NamespaceError> {
        if identify.nsze == 0 {
            return Err(NamespaceError::NotFound);
        }

        let lba_format_index = identify.flbas.low_index();
        if lba_format_index > identify.nlbaf {
            return Err(NamespaceError::FlbasInvalid);
        }

        let lbaf = identify.lbaf[lba_format_index as usize];
        let block_shift = lbaf.lbads();
        if !matches!(block_shift, 9..=16) {
            return Err(NamespaceError::LbaFormatInvalid(lbaf));
        }

        let max_transfer_block_count = {
            let mdts = if controller_identify.mdts != 0 {
                controller_identify.mdts
            } else {
                u8::MAX
            };
            let max_transfer_bits = mdts.saturating_add(NVME_PAGE_SHIFT);
            1 << max_transfer_bits
                .checked_sub(block_shift)
                .ok_or(NamespaceError::MdtsInvalid(mdts))?
                .min(16)
        };

        let preferred_deallocate_granularity = if identify.nsfeat.optperf() {
            identify.npdg
        } else {
            1
        };

        let reservation_capabilities = if controller_identify.oncs.reservations() {
            identify.rescap
        } else {
            nvm::ReservationCapabilities::new()
        };

        let state = Arc::new(DynamicState {
            block_count: identify.nsze.into(),
            removed: false.into(),
            identify: Mutex::new(identify),
            resize_event: Default::default(),
        });

        // NOTE: Detach `poll_for_rescans` task because its lifetime is not tied
        // to that of the Namespace object. `poll_for_rescans` terminates when the sender of
        // rescan_event is dropped. i.e. lifetime of this task is tied to the NvmeDriver
        // & `handle_asynchronous_events` task within the driver. Because the
        // driver stores references to Namespaces, this task will never outlive
        // the Namespace object.
        driver
            .spawn(format!("nvme_poll_rescan_{nsid}"), {
                let state = state.clone();
                async move { state.poll_for_rescans(&admin, nsid, rescan_event).await }
            })
            .detach();

        Ok(Self {
            nsid,
            state,
            max_transfer_block_count,
            block_shift: block_shift.into(),
            preferred_deallocate_granularity,
            reservation_capabilities,
            controller_identify,
            issuers: io_issuers.clone(),
        })
    }

    /// Gets the current block count.
    pub fn block_count(&self) -> u64 {
        self.state.block_count.load(Ordering::Relaxed)
    }

    /// Wait for the block count to be different from `block_count`.
    pub async fn wait_resize(&self, block_count: u64) -> u64 {
        loop {
            let listen = self.state.resize_event.listen();
            let current = self.block_count();
            if current != block_count {
                break current;
            }
            listen.await;
        }
    }

    /// Gets the block size in bytes.
    pub fn block_size(&self) -> u32 {
        1 << self.block_shift
    }

    pub fn check_active(&self) -> Result<(), RequestError> {
        if self.state.removed.load(Ordering::Relaxed) {
            // The namespace has been removed. Return invalid namespace even if
            // the namespace has returned to avoid accidentally accessing the
            // wrong disk.
            return Err(RequestError::Nvme(
                spec::Status::INVALID_NAMESPACE_OR_FORMAT.into(),
            ));
        }
        Ok(())
    }

    async fn issuer(&self, cpu: u32) -> Result<&Issuer, RequestError> {
        self.issuers.get(cpu).await
    }

    /// Reads from the namespace.
    pub async fn read(
        &self,
        target_cpu: u32,
        lba: u64,
        block_count: u32,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), RequestError> {
        self.check_active()?;
        if block_count == 0 {
            return Ok(());
        }
        assert!(block_count <= self.max_transfer_block_count);
        let len = (block_count as usize) << self.block_shift;
        if len > mem.len() {
            panic!(
                "invalid block count: {len} > {mem_len}",
                mem_len = mem.len()
            );
        }
        self.issuer(target_cpu)
            .await?
            .issue_external(
                spec::Command {
                    cdw10: nvm::Cdw10ReadWrite::new().with_sbla_low(lba as u32).into(),
                    cdw11: nvm::Cdw11ReadWrite::new()
                        .with_sbla_high((lba >> 32) as u32)
                        .into(),
                    cdw12: nvm::Cdw12ReadWrite::new()
                        .with_nlb_z((block_count - 1) as u16)
                        .into(),
                    ..nvm_cmd(nvm::NvmOpcode::READ, self.nsid)
                },
                guest_memory,
                mem.subrange(0, len),
            )
            .await?;
        Ok(())
    }

    /// Writes to the namespace.
    pub async fn write(
        &self,
        target_cpu: u32,
        lba: u64,
        block_count: u32,
        fua: bool,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), RequestError> {
        self.check_active()?;
        if block_count == 0 {
            return Ok(());
        }
        assert!(block_count <= self.max_transfer_block_count);
        let len = (block_count as usize) << self.block_shift;
        if len > mem.len() {
            panic!(
                "invalid block count: {len} > {mem_len}",
                mem_len = mem.len()
            );
        }
        self.issuer(target_cpu)
            .await?
            .issue_external(
                spec::Command {
                    cdw10: nvm::Cdw10ReadWrite::new().with_sbla_low(lba as u32).into(),
                    cdw11: nvm::Cdw11ReadWrite::new()
                        .with_sbla_high((lba >> 32) as u32)
                        .into(),
                    cdw12: nvm::Cdw12ReadWrite::new()
                        .with_nlb_z((block_count - 1) as u16)
                        .with_fua(fua)
                        .into(),
                    ..nvm_cmd(nvm::NvmOpcode::WRITE, self.nsid)
                },
                guest_memory,
                mem.subrange(0, len),
            )
            .await?;
        Ok(())
    }

    /// Flushes the namespace to persistent media.
    pub async fn flush(&self, target_cpu: u32) -> Result<(), RequestError> {
        self.check_active()?;
        self.issuer(target_cpu)
            .await?
            .issue_neither(spec::Command {
                ..nvm_cmd(nvm::NvmOpcode::FLUSH, self.nsid)
            })
            .await?;
        Ok(())
    }

    /// Returns the maximum size for a read or write, in blocks.
    pub fn max_transfer_block_count(&self) -> u32 {
        self.max_transfer_block_count
    }

    /// Returns whether the namespace support dataset management, needed to call
    /// [`Self::deallocate`].
    pub fn supports_dataset_management(&self) -> bool {
        self.controller_identify.oncs.dataset_management()
    }

    /// The preferred granularity for unmap requests.
    pub fn preferred_deallocate_granularity(&self) -> u16 {
        self.preferred_deallocate_granularity
    }

    /// Returns the maximum number of ranges to pass to [`Self::deallocate`].
    pub fn dataset_management_range_limit(&self) -> usize {
        // TODO: query DMRL
        256
    }

    /// Returns the maximum size of a single range to pass to
    /// [`Self::deallocate`].
    pub fn dataset_management_range_size_limit(&self) -> u32 {
        // TODO: query DMRSL
        u32::MAX
    }

    /// Issues a dataset management command to deallocate the specified ranges.
    ///
    /// The device may ignore ranges or LBA counts beyond a certain point. Use
    /// [`Self::dataset_management_range_limit`] and
    /// [`Self::dataset_management_range_size_limit`] to get the
    /// controller-reported bounds.
    pub async fn deallocate(
        &self,
        target_cpu: u32,
        ranges: &[nvm::DsmRange],
    ) -> Result<(), RequestError> {
        self.check_active()?;
        // Limit the requested ranges.
        let ranges = &ranges[..ranges.len().min(256)];
        self.issuer(target_cpu)
            .await?
            .issue_in(
                spec::Command {
                    cdw10: nvm::Cdw10Dsm::new()
                        .with_nr_z((ranges.len() - 1) as u8)
                        .into(),
                    cdw11: nvm::Cdw11Dsm::new().with_ad(true).into(),
                    ..nvm_cmd(nvm::NvmOpcode::DSM, self.nsid)
                },
                ranges.as_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Gets the namespace's reservation capabilities.
    pub fn reservation_capabilities(&self) -> nvm::ReservationCapabilities {
        self.reservation_capabilities
    }

    /// Gets the namespace's reservation report.
    pub async fn reservation_report_extended(
        &self,
        target_cpu: u32,
    ) -> Result<
        (
            nvm::ReservationReportExtended,
            Vec<nvm::RegisteredControllerExtended>,
        ),
        RequestError,
    > {
        let mut data = vec![0; 4096];
        let issuer = self.issuer(target_cpu).await?;
        loop {
            issuer
                .issue_out(
                    spec::Command {
                        cdw10: nvm::Cdw10ReservationReport::new()
                            .with_numd_z((data.len() / 4 - 1) as u32)
                            .into(),
                        cdw11: nvm::Cdw11ReservationReport::new().with_eds(true).into(),
                        ..nvm_cmd(nvm::NvmOpcode::RESERVATION_REPORT, self.nsid)
                    },
                    &mut data,
                )
                .await?;

            let header = nvm::ReservationReportExtended::read_from_prefix(&data[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            let len = size_of_val(&header)
                + header.report.regctl.get() as usize
                    * size_of::<nvm::RegisteredControllerExtended>();

            if len > data.len() {
                data.resize(len, 0);
                continue;
            }

            let mut controllers = vec![
                nvm::RegisteredControllerExtended::new_zeroed();
                header.report.regctl.get().into()
            ];

            controllers
                .as_mut_bytes()
                .copy_from_slice(&data[size_of_val(&header)..len]);

            break Ok((header, controllers));
        }
    }

    /// Acquires a reservation.
    pub async fn reservation_acquire(
        &self,
        target_cpu: u32,
        action: nvm::ReservationAcquireAction,
        crkey: u64,
        prkey: u64,
        reservation_type: nvm::ReservationType,
    ) -> Result<(), RequestError> {
        let data = nvm::ReservationAcquire { crkey, prkey };
        self.issuer(target_cpu)
            .await?
            .issue_in(
                spec::Command {
                    cdw10: nvm::Cdw10ReservationAcquire::new()
                        .with_racqa(action.0)
                        .with_rtype(reservation_type.0)
                        .into(),
                    ..nvm_cmd(nvm::NvmOpcode::RESERVATION_ACQUIRE, self.nsid)
                },
                data.as_bytes(),
            )
            .await?;

        Ok(())
    }

    /// Releases a reservation.
    pub async fn reservation_release(
        &self,
        target_cpu: u32,
        action: nvm::ReservationReleaseAction,
        crkey: u64,
        reservation_type: nvm::ReservationType,
    ) -> Result<(), RequestError> {
        let data = nvm::ReservationRelease { crkey };
        self.issuer(target_cpu)
            .await?
            .issue_in(
                spec::Command {
                    cdw10: nvm::Cdw10ReservationRelease::new()
                        .with_rrela(action.0)
                        .with_rtype(reservation_type.0)
                        .into(),
                    ..nvm_cmd(nvm::NvmOpcode::RESERVATION_RELEASE, self.nsid)
                },
                data.as_bytes(),
            )
            .await?;

        Ok(())
    }

    /// Modifies a reservation registration.
    pub async fn reservation_register(
        &self,
        target_cpu: u32,
        action: nvm::ReservationRegisterAction,
        crkey: Option<u64>,
        nrkey: u64,
        ptpl: Option<bool>,
    ) -> Result<(), RequestError> {
        let data = nvm::ReservationRegister {
            crkey: crkey.unwrap_or(0),
            nrkey,
        };
        let cptpl = match ptpl {
            None => nvm::ChangePersistThroughPowerLoss::NO_CHANGE,
            Some(false) => nvm::ChangePersistThroughPowerLoss::CLEAR,
            Some(true) => nvm::ChangePersistThroughPowerLoss::SET,
        };
        self.issuer(target_cpu)
            .await?
            .issue_in(
                spec::Command {
                    cdw10: nvm::Cdw10ReservationRegister::new()
                        .with_rrega(action.0)
                        .with_iekey(crkey.is_none())
                        .with_cptpl(cptpl.0)
                        .into(),
                    ..nvm_cmd(nvm::NvmOpcode::RESERVATION_REGISTER, self.nsid)
                },
                data.as_bytes(),
            )
            .await?;

        Ok(())
    }

    /// Return Namespace ID.
    pub fn nsid(&self) -> u32 {
        self.nsid
    }

    /// Save namespace object data for servicing.
    /// Initially we will re-query namespace state after restore
    /// to avoid possible contention if namespace was changed
    /// during servicing.
    pub fn save(&self) -> anyhow::Result<SavedNamespaceData> {
        Ok(SavedNamespaceData {
            nsid: self.nsid,
            identify_ns: self.state.identify.lock().clone(),
        })
    }

    /// Restore namespace object data after servicing.
    pub(super) fn restore(
        driver: &VmTaskDriver,
        admin: Arc<Issuer>,
        rescan_event: mesh::Receiver<()>,
        identify_ctrl: Arc<spec::IdentifyController>,
        io_issuers: &Arc<IoIssuers>,
        saved_state: &SavedNamespaceData,
    ) -> Result<Self, NamespaceError> {
        let SavedNamespaceData { nsid, identify_ns } = saved_state;

        Namespace::new_from_identify(
            driver,
            admin,
            rescan_event,
            identify_ctrl.clone(),
            io_issuers,
            *nsid,
            identify_ns.clone(),
        )
    }
}

impl DynamicState {
    async fn poll_for_rescans(
        &self,
        admin: &Issuer,
        nsid: u32,
        mut rescan_event: mesh::Receiver<()>,
    ) {
        loop {
            tracing::debug!("rescan");

            // This relies on a mesh channel so notifications will NOT be missed
            // even if the task was not started when the first AEN was processed.
            let event = rescan_event.next().await;

            // Once the sender is dropped, no more repoll signals can be received so
            // there is no point in continuing.
            if event.is_none() {
                tracing::debug!("rescan task exiting");
                break;
            }

            match identify_namespace(admin, nsid).await {
                Ok(identify) => {
                    if identify.nsze == 0 {
                        tracing::info!(nsid, "namespace was hot removed");
                        self.removed.store(true, Ordering::Relaxed);
                    } else {
                        let old_block_count = self.block_count.load(Ordering::Relaxed);
                        let new_block_count = identify.nsze;
                        if old_block_count != new_block_count {
                            tracing::info!(
                                old_block_count,
                                new_block_count,
                                "nvme disk size changed"
                            );
                            self.block_count.store(new_block_count, Ordering::Relaxed);
                            self.resize_event.notify(usize::MAX);
                        } else {
                            tracing::debug!("rescanned, no change");
                        }
                    }
                    *self.identify.lock() = identify;
                }
                Err(err) => {
                    tracing::warn!(
                        nsid,
                        error = &err as &dyn std::error::Error,
                        "failed to query namespace during rescan"
                    );
                }
            }
        }
    }
}

async fn identify_namespace(
    admin: &Issuer,
    nsid: u32,
) -> Result<nvm::IdentifyNamespace, RequestError> {
    let mut identify = nvm::IdentifyNamespace::new_zeroed();
    admin
        .issue_out(
            spec::Command {
                nsid,
                cdw10: spec::Cdw10Identify::new()
                    .with_cns(spec::Cns::NAMESPACE.0)
                    .into(),
                ..admin_cmd(spec::AdminOpcode::IDENTIFY)
            },
            identify.as_mut_bytes(),
        )
        .await?;
    Ok(identify)
}

fn nvm_cmd(opcode: nvm::NvmOpcode, nsid: u32) -> spec::Command {
    spec::Command {
        cdw0: spec::Cdw0::new().with_opcode(opcode.0),
        nsid,
        ..FromZeros::new_zeroed()
    }
}
