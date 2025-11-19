// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The interface to the MANA device.

pub use crate::bnic_driver::RxConfig;
pub use crate::resources::ResourceArena;
pub use crate::save_restore::ManaDeviceSavedState;

use crate::bnic_driver::BnicDriver;
use crate::bnic_driver::WqConfig;
use crate::gdma_driver::GdmaDriver;
use crate::queues;
use crate::queues::Doorbell;
use crate::queues::DoorbellPage;
use anyhow::Context;
use futures::StreamExt;
use futures::lock::Mutex;
use gdma_defs::GdmaDevId;
use gdma_defs::GdmaDevType;
use gdma_defs::GdmaQueueType;
use gdma_defs::GdmaRegisterDeviceResp;
use gdma_defs::bnic::ManaQueryDeviceCfgResp;
use gdma_defs::bnic::ManaQueryFilterStateResponse;
use gdma_defs::bnic::ManaQueryStatisticsResponse;
use gdma_defs::bnic::ManaQueryVportCfgResp;
use gdma_defs::bnic::STATISTICS_FLAGS_ALL;
use inspect::Inspect;
use net_backend_resources::mac_address::MacAddress;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::sync::Arc;
use tracing::Instrument;
use user_driver::DeviceBacking;
use user_driver::DmaClient;
use user_driver::DmaPool;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use vmcore::vm_task::VmTaskDriverSource;

enum LinkStatus {
    Default,
    Pending(bool),
    Active {
        sender: mesh::Sender<bool>,
        connected: bool,
    },
}

/// A MANA device.
#[derive(Inspect)]
pub struct ManaDevice<T: DeviceBacking> {
    #[inspect(skip)]
    inner: Arc<Inner<T>>,
    #[inspect(skip)]
    inspect_task: Task<()>,
    #[inspect(skip)]
    hwc_task: Option<Task<()>>,
    #[inspect(flatten, send = "|x| x")]
    inspect_send: mesh::Sender<inspect::Deferred>,
}

struct Inner<T: DeviceBacking> {
    gdma: Mutex<GdmaDriver<T>>,
    dev_id: GdmaDevId,
    dev_data: GdmaRegisterDeviceResp,
    dev_config: ManaQueryDeviceCfgResp,
    doorbell: Arc<dyn Doorbell>,
    vport_link_status: Arc<Mutex<Vec<LinkStatus>>>,
}

impl<T: DeviceBacking> ManaDevice<T> {
    /// Initializes the MANA driver on `device`.
    pub async fn new(
        driver: &impl SpawnDriver,
        device: T,
        num_vps: u32,
        max_queues_per_vport: u16,
        mana_state: Option<&ManaDeviceSavedState>,
    ) -> anyhow::Result<Self> {
        let mut gdma = if let Some(mana_state) = mana_state {
            let memory = device.dma_client().attach_pending_buffers()?;
            let gdma_memory = memory
                .iter()
                .find(|m| m.pfns()[0] == mana_state.gdma.mem.base_pfn)
                .expect("gdma restored memory not found")
                .clone();

            GdmaDriver::restore(mana_state.gdma.clone(), device, gdma_memory)
                .instrument(tracing::info_span!("restore_gdma_driver"))
                .await?
        } else {
            GdmaDriver::new(driver, device, num_vps, None)
                .instrument(tracing::info_span!("new_gdma_driver"))
                .await?
        };

        gdma.test_eq().await?;

        gdma.verify_vf_driver_version().await?;

        let dev_id = gdma
            .list_devices()
            .await?
            .iter()
            .copied()
            .find(|dev_id| dev_id.ty == GdmaDevType::GDMA_DEVICE_MANA)
            .context("no mana device found")?;

        let dev_data = if let Some(mana_state) = mana_state {
            GdmaRegisterDeviceResp {
                pdid: mana_state.gdma.pdid,
                gpa_mkey: mana_state.gdma.gpa_mkey,
                db_id: mana_state.gdma.db_id as u32,
            }
        } else {
            gdma.register_device(dev_id).await?
        };

        let mut bnic = BnicDriver::new(&mut gdma, dev_id);
        let dev_config = bnic.query_dev_config().await?;
        tracing::info!(mana_dev_config = ?dev_config);
        let num_queues_needed = dev_config.max_num_vports as u32 * max_queues_per_vport as u32;
        gdma.check_vf_resources(num_vps, num_queues_needed);

        let doorbell = gdma.doorbell();
        let vport_link_status = (0..dev_config.max_num_vports)
            .map(|_| LinkStatus::Default)
            .collect();
        let inner = Arc::new(Inner {
            gdma: Mutex::new(gdma),
            dev_id,
            dev_data,
            dev_config,
            doorbell,
            vport_link_status: Arc::new(Mutex::new(vport_link_status)),
        });

        let (inspect_send, mut inspect_recv) = mesh::channel::<inspect::Deferred>();
        let inspect_task = driver.spawn("mana-inspect", {
            let inner = inner.clone();
            async move {
                while let Some(deferred) = inspect_recv.next().await {
                    let Inner {
                        gdma,
                        dev_id: _,
                        dev_data: _,
                        dev_config: _,
                        doorbell: _,
                        vport_link_status: _,
                    } = inner.as_ref();
                    let gdma = gdma.lock().await;
                    deferred.respond(|resp| {
                        resp.merge(&*gdma);
                    })
                }
            }
        });

        let device = Self {
            inner,
            inspect_send,
            inspect_task,
            hwc_task: None,
        };
        Ok(device)
    }

    /// Saves the device's state for servicing
    pub async fn save(self) -> (anyhow::Result<ManaDeviceSavedState>, T) {
        self.inspect_task.cancel().await;
        if let Some(hwc_task) = self.hwc_task {
            hwc_task.cancel().await;
        }
        let inner = Arc::into_inner(self.inner).unwrap();
        let mut driver = inner.gdma.into_inner();

        if let Ok(saved_state) = driver.save().await {
            let mana_saved_state = ManaDeviceSavedState { gdma: saved_state };

            (Ok(mana_saved_state), driver.into_device())
        } else {
            tracing::error!("Failed to save MANA device state");
            (
                Err(anyhow::anyhow!("Failed to save MANA device state")),
                driver.into_device(),
            )
        }
    }

    /// Returns the number of vports the device supports.
    pub fn num_vports(&self) -> u32 {
        self.inner.dev_config.max_num_vports.into()
    }

    /// Returns the device configuration.
    pub fn dev_config(&self) -> &ManaQueryDeviceCfgResp {
        &self.inner.dev_config
    }

    /// Starts a hardware channel (HWC) task that listens to events on the HWC
    /// and calls the appropriate provided callsbacks/closure.
    pub async fn start_notification_task(&mut self, driver_source: &VmTaskDriverSource) {
        if self.hwc_task.is_some() {
            return;
        }

        let inner = self.inner.clone();
        let hwc_task = driver_source.simple().spawn("mana-hwc", {
            let mut gdma = self.inner.gdma.lock().await;
            let mut hwc_event = gdma.hwc_subscribe();
            async move {
                loop {
                    hwc_event.wait().await;
                    let mut gdma = inner.gdma.lock().await;
                    if gdma.process_all_eqs() {
                        let mut vport_link_status = inner.vport_link_status.lock().await;
                        for (vport_index, current) in gdma.get_link_toggle_list() {
                            let vport_index = vport_index as usize;
                            if vport_index >= vport_link_status.len() {
                                tracing::error!(vport_index, "Invalid vport index");
                                continue;
                            }
                            if let LinkStatus::Active { sender, connected } =
                                &mut vport_link_status[vport_index]
                            {
                                *connected = current;
                                sender.send(*connected);
                            } else {
                                let _ = std::mem::replace(
                                    &mut vport_link_status[vport_index],
                                    LinkStatus::Pending(current),
                                );
                            }
                        }
                    }
                }
            }
        });
        self.hwc_task = Some(hwc_task);
    }

    /// Initializes and returns the vport number `index`.
    pub async fn new_vport(
        &self,
        index: u32,
        vport_state: Option<VportState>,
        dev_config: &ManaQueryDeviceCfgResp,
    ) -> anyhow::Result<Vport<T>> {
        let vport_config = self.query_vport_config(index).await?;

        let vport_state = vport_state.unwrap_or(VportState::new(None, None));

        let vport = Vport {
            inner: self.inner.clone(),
            config: vport_config,
            vport_state,
            id: index,
        };

        if dev_config.cap_filter_state_query() {
            if let Ok(resp) = vport.query_filter_state(vport.id.into()).await {
                tracing::debug!(
                    mac_address = %vport.mac_address(),
                    direction_to_vtl0 = resp.direction_to_vtl0,
                    "query_filter_state"
                );
                vport
                    .vport_state
                    .set_direction_to_vtl0(resp.direction_to_vtl0 == 1);
            }
        }

        Ok(vport)
    }

    /// Shuts the device down.
    pub async fn shutdown(self) -> (anyhow::Result<()>, T) {
        self.inspect_task.cancel().await;
        if let Some(hwc_task) = self.hwc_task {
            hwc_task.cancel().await;
        }
        let inner = Arc::into_inner(self.inner).unwrap();
        let mut driver = inner.gdma.into_inner();
        let result = driver.deregister_device(inner.dev_id).await;
        (result, driver.into_device())
    }
    /// Queries the configuration of a specific vport.
    pub async fn query_vport_config(&self, vport: u32) -> anyhow::Result<ManaQueryVportCfgResp> {
        let mut gdma = self.inner.gdma.lock().await;
        BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .query_vport_config(vport)
            .await
    }
}

/// Tracks vport state and optionally notifies a listener of changes.
#[derive(Clone)]
pub struct VportState {
    direction_to_vtl0: Arc<parking_lot::Mutex<Option<bool>>>,
    state_change_callback: Arc<Option<Box<dyn Fn(bool) + Send + Sync>>>,
}

impl VportState {
    /// Create a new VportState instance.
    pub fn new(
        direction_to_vtl0: Option<bool>,
        state_change_callback: Option<Box<dyn Fn(bool) + Send + Sync>>,
    ) -> Self {
        Self {
            direction_to_vtl0: Arc::new(parking_lot::Mutex::new(direction_to_vtl0)),
            state_change_callback: Arc::new(state_change_callback),
        }
    }

    /// Remember current filter setting.
    pub fn set_direction_to_vtl0(&self, direction_to_vtl0: bool) {
        *self.direction_to_vtl0.lock() = Some(direction_to_vtl0);
        if let Some(callback) = self.state_change_callback.as_ref() {
            (callback)(direction_to_vtl0);
        }
    }

    /// Get current filter setting if known.
    pub fn get_direction_to_vtl0(&self) -> Option<bool> {
        *self.direction_to_vtl0.lock()
    }
}

/// A MANA vport.
pub struct Vport<T: DeviceBacking> {
    inner: Arc<Inner<T>>,
    config: ManaQueryVportCfgResp,
    vport_state: VportState,
    id: u32,
}

impl<T: DeviceBacking> Vport<T> {
    /// Returns the maximum number of transmit queues.
    pub fn max_tx_queues(&self) -> u32 {
        self.config.max_num_sq
    }

    /// Returns the maximum number of receive queues.
    pub fn max_rx_queues(&self) -> u32 {
        self.config.max_num_rq
    }

    /// Returns the assigned MAC address.
    pub fn mac_address(&self) -> MacAddress {
        self.config.mac_addr.into()
    }

    /// Returns the memory key to refer to all of GPA space.
    pub fn gpa_mkey(&self) -> u32 {
        self.inner.dev_data.gpa_mkey
    }

    /// Returns this vport's id
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Returns the number of indirection entries supported by the vport
    pub fn num_indirection_ent(&self) -> u32 {
        self.config.num_indirection_ent
    }

    /// Creates a new event queue.
    pub async fn new_eq(
        &self,
        arena: &mut ResourceArena,
        size: u32,
        cpu: u32,
    ) -> anyhow::Result<BnicEq> {
        let mut gdma = self.inner.gdma.lock().await;
        let dma_client = gdma.device().dma_client_for(DmaPool::Ephemeral)?;
        let mem = dma_client
            .allocate_dma_buffer(size as usize)
            .context("Failed to allocate DMA buffer")?;

        let gdma_region = gdma
            .create_dma_region(arena, self.inner.dev_id, mem.clone())
            .await
            .context("failed to create eq dma region")?;
        let (id, interrupt) = gdma
            .create_eq(
                arena,
                self.inner.dev_id,
                gdma_region,
                size,
                self.inner.dev_data.pdid,
                self.inner.dev_data.db_id,
                cpu,
            )
            .await
            .context("failed to create eq")?;
        Ok(BnicEq {
            doorbell: DoorbellPage::new(self.inner.doorbell.clone(), self.inner.dev_data.db_id)?,
            mem,
            id,
            interrupt,
        })
    }

    /// Creates a new work queue (transmit or receive).
    pub async fn new_wq(
        &self,
        arena: &mut ResourceArena,
        is_send: bool,
        wq_size: u32,
        cq_size: u32,
        eq_id: u32,
    ) -> anyhow::Result<BnicWq> {
        assert!(wq_size >= PAGE_SIZE as u32 && wq_size.is_power_of_two());
        assert!(cq_size >= PAGE_SIZE as u32 && cq_size.is_power_of_two());
        let mut gdma = self.inner.gdma.lock().await;

        let dma_client = gdma.device().dma_client_for(DmaPool::Ephemeral)?;

        let mem = dma_client
            .allocate_dma_buffer((wq_size + cq_size) as usize)
            .context("failed to allocate DMA buffer")?;

        let wq_mem = mem.subblock(0, wq_size as usize);
        let cq_mem = mem.subblock(wq_size as usize, cq_size as usize);

        let wq_gdma_region = gdma
            .create_dma_region(arena, self.inner.dev_id, wq_mem.clone())
            .await?;
        let cq_gdma_region = gdma
            .create_dma_region(arena, self.inner.dev_id, cq_mem.clone())
            .await?;
        let wq_type = if is_send {
            GdmaQueueType::GDMA_SQ
        } else {
            GdmaQueueType::GDMA_RQ
        };
        let doorbell = DoorbellPage::new(self.inner.doorbell.clone(), self.inner.dev_data.db_id)?;
        let resp = BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .create_wq_obj(
                arena,
                self.config.vport,
                wq_type,
                &WqConfig {
                    wq_gdma_region,
                    cq_gdma_region,
                    wq_size,
                    cq_size,
                    cq_moderation_ctx_id: 0,
                    eq_id,
                },
            )
            .await?;

        Ok(BnicWq {
            doorbell,
            wq_mem,
            cq_mem,
            wq_id: resp.wq_id,
            cq_id: resp.cq_id,
            is_send,
            wq_obj: resp.wq_obj,
        })
    }

    /// Get the transmit configuration.
    pub async fn config_tx(&self) -> anyhow::Result<TxConfig> {
        let mut gdma = self.inner.gdma.lock().await;
        let resp = BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .config_vport_tx(
                self.config.vport,
                self.inner.dev_data.pdid,
                self.inner.dev_data.db_id,
            )
            .await?;

        let config = TxConfig {
            tx_vport_offset: resp.tx_vport_offset,
        };
        Ok(config)
    }

    /// Sets the receive configuration.
    pub async fn config_rx(&self, config: &RxConfig<'_>) -> anyhow::Result<()> {
        let mut gdma = self.inner.gdma.lock().await;
        BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .config_vport_rx(self.config.vport, config)
            .await?;

        Ok(())
    }

    /// Move filter between VTL2 VF vport and VTL0 VF vport
    pub async fn move_filter(&self, direction_to_vtl0: u8) -> anyhow::Result<()> {
        if let Some(to_vtl0) = self.vport_state.get_direction_to_vtl0() {
            if to_vtl0 == (direction_to_vtl0 == 1) {
                return Ok(());
            }
        }
        let mut gdma = self.inner.gdma.lock().await;
        let hwc_activity_id = BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .move_vport_filter(self.config.vport, direction_to_vtl0)
            .await?;
        self.vport_state
            .set_direction_to_vtl0(direction_to_vtl0 == 1);
        tracing::info!(
            mac_address = %self.mac_address(),
            direction_to_vtl0,
            hwc_activity_id,
            "switch data path for mac",
        );
        Ok(())
    }

    /// Get current filter state.
    pub async fn get_direction_to_vtl0(&self) -> Option<bool> {
        self.vport_state.get_direction_to_vtl0()
    }

    /// Set the vport serial number
    pub async fn set_serial_no(&self, serial_no: u32) -> anyhow::Result<()> {
        let mut gdma = self.inner.gdma.lock().await;
        BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .set_vport_serial_no(self.config.vport, serial_no)
            .await?;
        Ok(())
    }

    /// Gets stats. Note that these are adapter-wide and not really per-vport.
    pub async fn query_stats(&self) -> anyhow::Result<ManaQueryStatisticsResponse> {
        let mut gdma = self.inner.gdma.lock().await;
        BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .query_stats(STATISTICS_FLAGS_ALL)
            .await
    }

    /// Retrieves vport mac filter state from socamana
    pub async fn query_filter_state(
        &self,
        vport: u64,
    ) -> anyhow::Result<ManaQueryFilterStateResponse> {
        let mut gdma = self.inner.gdma.lock().await;
        BnicDriver::new(&mut *gdma, self.inner.dev_id)
            .query_filter_state(vport)
            .await
    }

    /// Destroys resources in `arena`.
    pub async fn destroy(&self, arena: ResourceArena) {
        let mut gdma = self.inner.gdma.lock().await;
        arena.destroy(&mut *gdma).await;
    }

    /// Changes the target CPU for the given eq to `cpu`.
    pub async fn retarget_interrupt(
        &self,
        eq_id: u32,
        cpu: u32,
    ) -> anyhow::Result<Option<DeviceInterrupt>> {
        let mut gdma = self.inner.gdma.lock().await;
        gdma.retarget_eq(self.inner.dev_id, eq_id, cpu).await
    }

    /// Registers for link status notification updates.
    pub async fn register_link_status_notifier(&self, sender: mesh::Sender<bool>) {
        let mut vport_link_status = self.inner.vport_link_status.lock().await;
        let vport_index = self.id as usize;
        let (send, connected) = match vport_link_status[vport_index] {
            // Send any pending notifications, whatever it is.
            LinkStatus::Pending(connected) => (true, connected),
            // Endpoint reestablishing connection. Only send, if the link is down.
            LinkStatus::Active { connected, .. } => (!connected, connected),
            // Don't send anything when transitioning from the default state.
            _ => (false, true),
        };
        if send {
            sender.send(connected);
        }
        vport_link_status[vport_index] = LinkStatus::Active { sender, connected };
    }

    /// Returns an object that can allocate dma memory to be shared with the device.
    pub async fn dma_client(&self) -> Arc<dyn DmaClient> {
        self.inner.gdma.lock().await.device().dma_client()
    }
}

/// Transmit configuration.
pub struct TxConfig {
    /// The vport offset to include in tx packets.
    pub tx_vport_offset: u16,
}

/// An event queue.
pub struct BnicEq {
    doorbell: DoorbellPage,
    mem: MemoryBlock,
    id: u32,
    interrupt: DeviceInterrupt,
}

impl BnicEq {
    /// The event queue ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The interrupt that will be signaled when the armed event queue is ready.
    pub fn interrupt(&self) -> DeviceInterrupt {
        self.interrupt.clone()
    }

    /// Gets an object to access the queue's entries.
    pub fn queue(&self) -> queues::Eq {
        queues::Eq::new_eq(self.mem.clone(), self.doorbell.clone(), self.id)
    }
}

/// A work queue (transmit or receive).
pub struct BnicWq {
    doorbell: DoorbellPage,
    wq_mem: MemoryBlock,
    cq_mem: MemoryBlock,
    wq_id: u32,
    cq_id: u32,
    is_send: bool,
    wq_obj: u64,
}

impl BnicWq {
    /// Gets the work queue for sending requests.
    pub fn wq(&self) -> queues::Wq {
        if self.is_send {
            queues::Wq::new_sq(self.wq_mem.clone(), self.doorbell.clone(), self.wq_id)
        } else {
            queues::Wq::new_rq(self.wq_mem.clone(), self.doorbell.clone(), self.wq_id)
        }
    }

    /// Gets the completion queue for receiving results.
    pub fn cq(&self) -> queues::Cq {
        queues::Cq::new_cq(self.cq_mem.clone(), self.doorbell.clone(), self.cq_id)
    }

    /// Gets the work queue object ID.
    pub fn wq_obj(&self) -> u64 {
        self.wq_obj
    }
}
