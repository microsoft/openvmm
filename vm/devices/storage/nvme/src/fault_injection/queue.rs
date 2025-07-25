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
    gpa: u64,
    #[inspect(skip)]
    controller: Arc<Mutex<NvmeController>>,
    #[inspect(skip)]
    doorbell_write: mesh::Cell<(u16, u32)>,
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
            gpa,
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
        let head_cached = self.inner.sqhd();
        // let mut changed_command = spec::Command::default();
        let command = self.inner.next(mem).await?;
        // changed_command = command.clone();
        // changed_command.cdw0.set_opcode(0x06);
        tracing::debug!(
            "GETTING THE NEXT OPERATION IN THE CHAIN with gpa: {:#x}",
            self.gpa
        );
        // mem.write_plain(
        //     self.gpa.wrapping_add(head_cached as u64 * 64),
        //     &changed_command,
        // )
        // .map_err(QueueError::Memory)?;
        let (addr, data) = self.doorbell_write.get();
        let mut inner_controller = self.controller.lock();
        tracing::debug!("INVOKING INNER WRITE BAR0 FOR DOORBELL WITH DATA {addr}, data: {data:?}");
        let data = u32::to_ne_bytes(data);
        inner_controller.write_bar0(addr, &data);
        Ok(command)
    }

    pub fn sqhd(&self) -> u16 {
        self.inner.sqhd()
    }

    /// This function lets the driver know what doorbell value we consumed, allowing
    /// it to elide the next ring, maybe.
    pub fn advance_evt_idx(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        self.inner.advance_evt_idx(mem)
    }

    /// This function updates the shadow doorbell values of a queue that is
    /// potentially already in use.
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.inner.update_shadow_db(mem, sdb)
    }
}
