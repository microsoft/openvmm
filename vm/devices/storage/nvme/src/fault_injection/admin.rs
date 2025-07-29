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
use vmcore::vm_task::VmTaskDriver;

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
    #[inspect(skip)]
    pub mem: GuestMemory,
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
        // TODO: Why is the inner controller handling 3 different types of events? This seems to work for now.
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
    pub fn new(handler: &AdminHandlerFaultInjection, asq: u64, asqs: u16) -> Self {
        Self {
            admin_sq: SubmissionQueueFaultInjection::new(
                handler.config.doorbells[0].clone(),
                asq,
                asqs,
                None,
                handler.config.controller.clone(),
            ),
        }
    }
}
