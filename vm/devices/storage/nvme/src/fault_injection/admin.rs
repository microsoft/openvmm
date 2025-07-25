use crate::NvmeController;
use crate::fault_injection::queue::SubmissionQueueFaultInjection;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::spec;
use crate::workers::IoQueueEntrySizes;
use futures::FutureExt;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(Debug)]
enum Event {
    Command(Result<spec::Command, QueueError>), // TODO: Is this really needed?
}

/// An admin handler shim layer for fault injection.
#[derive(Inspect)]
pub(crate) struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfigFaultInjection,
}

#[derive(Inspect)]
pub(crate) struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueueFaultInjection,
}

#[derive(Inspect)]
pub(crate) struct AdminConfigFaultInjection {
    #[inspect(skip)] // TODO: What all do we want to be able to inspect?
    pub driver_source: VmTaskDriverSource, // TODO: Do we need to keep this?
    #[inspect(skip)]
    pub mem: GuestMemory,
    pub inner_mem: GuestMemory,
    #[inspect(skip)]
    pub doorbells: Vec<Arc<DoorbellRegister>>,
    #[inspect(display)]
    pub subsystem_id: Guid,
    pub max_sqs: u16,
    pub max_cqs: u16,
    pub qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(skip)]
    pub controller: Arc<Mutex<NvmeController>>,
}

impl AsyncRun<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), Cancelled> {
        loop {
            stop.until_stopped(self.next_event(state)).await?;
        }
        Ok(())
    }
}

impl AdminHandlerFaultInjection {
    async fn next_event(
        &mut self,
        state: &mut AdminStateFaultInjection,
    ) -> Result<Event, QueueError> {
        // A little bit of an explanation here: From the looks of it, the underlying admin handler
        // is actually handling 3 different types of commands. The sq_delete_response, admin_sq, and changed_namespace.
        // For now we are only concerned with the admin_sq because that is the driver->controller communication that we are interested in.
        let next_command = state
            .admin_sq
            .next(&self.config.mem)
            .map(Event::Command)
            .await;
        Ok(next_command)
    }
}

impl AdminHandlerFaultInjection {
    pub fn new(driver: VmTaskDriver, config: AdminConfigFaultInjection) -> Self {
        Self { driver, config }
    }
}

impl InspectTask<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&AdminStateFaultInjection>) {
        req.respond().merge(self).merge(state);
    }
}

impl AdminStateFaultInjection {
    pub fn new(
        handler: &AdminHandlerFaultInjection,
        asq: u64,
        asqs: u16,
        doorbell_write: mesh::Cell<(u16, u32)>,
    ) -> Self {
        Self {
            admin_sq: SubmissionQueueFaultInjection::new(
                handler.config.doorbells[0].clone(),
                asq,
                asqs,
                None,
                handler.config.controller.clone(),
                doorbell_write,
            ),
        }
    }
}
