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

#[derive(Inspect)]
pub struct SubmissionQueueFaultInjection {
    inner: SubmissionQueue,
    #[inspect(skip)]
    controller: Arc<Mutex<NvmeController>>,
    #[inspect(skip)]
    doorbell_write: mesh::Cell<(u16, u32)>, // TODO: If it knows it's own address and the tail to ding, we don't need this anymore
}

impl SubmissionQueueFaultInjection {
    pub fn new(
        tail: Arc<DoorbellRegister>,
        gpa: u64,
        len: u16,
        shadow_db_evt_idx: Option<ShadowDoorbell>,
        controller: Arc<Mutex<NvmeController>>,
        doorbell_write: mesh::Cell<(u16, u32)>,
    ) -> Self {
        Self {
            inner: SubmissionQueue::new(tail, gpa, len, shadow_db_evt_idx),
            controller,
            doorbell_write,
        }
    }

    /// This function returns a future for the next entry in the submission queue.  It also
    /// has a side effect of updating the tail.
    ///
    /// Note that this function returns a future that must be cancellable, which means that the
    /// parts after an await may never run.  The tail update side effect is benign, so
    /// that can happen before the await.
    /// TODO: This approach will only work for a single admin command at a time. If multiple commands
    /// are placed at the same time, this will not work as expected!
    pub async fn next(&mut self, mem: &GuestMemory) -> Result<spec::Command, QueueError> {
        let command = self.inner.next(mem).await?;
        let (addr, data) = self.doorbell_write.get();
        let mut inner_controller = self.controller.lock();
        let data = u32::to_ne_bytes(data);
        inner_controller.write_bar0(addr, &data);
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

    /// Passthorugh
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.inner.update_shadow_db(mem, sdb)
    }
}
