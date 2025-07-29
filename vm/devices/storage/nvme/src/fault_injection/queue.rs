use crate::NvmeController;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::ShadowDoorbell;
use crate::queue::SubmissionQueue;
use crate::spec;
use guestmem::GuestMemory;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::info;

#[derive(Inspect)]
pub struct SubmissionQueueFaultInjection {
    #[inspect(skip)]
    controller: Arc<Mutex<NvmeController>>,
    inner: SubmissionQueue,
    addr: u16, // The address of the submission queue in the device's BAR0.
}

impl SubmissionQueueFaultInjection {
    pub fn new(
        tail: Arc<DoorbellRegister>,
        gpa: u64,
        len: u16,
        shadow_db_evt_idx: Option<ShadowDoorbell>,
        controller: Arc<Mutex<NvmeController>>,
    ) -> Self {
        let gpa_offset: u16 = 0x1000; // TODO: This is a hack to get the address. This needs to be included in the address that is sent over.
        Self {
            inner: SubmissionQueue::new(tail, gpa, len, shadow_db_evt_idx),
            addr: gpa_offset.wrapping_add(gpa as u16),
            controller,
        }
    }

    /// Returns the next command in the submission queue and provides functionality to
    /// throttle or alter the contents of the Admin submission queue.
    /// Throttling is done by issuing a doorbell write to the inner controller and always providing
    /// a new tail of head + 1.
    pub async fn next(&mut self, mem: &GuestMemory) -> Result<spec::Command, QueueError> {
        let command = self.inner.next(mem).await?;
        let data = self.sqhd() as u32; // Ensures 1 doorbell write per command!
        let mut inner_controller = self.controller.lock();
        let data = u32::to_ne_bytes(data);

        inner_controller.write_bar0(self.addr, &data);
        Ok(command)
    }

    /// Passthrough
    pub fn sqhd(&self) -> u16 {
        self.inner.sqhd()
    }

    /// Passthrough
    pub fn advance_evt_idx(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        self.inner.advance_evt_idx(mem)
    }

    /// Passthrough
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.inner.update_shadow_db(mem, sdb)
    }
}
