use crate::NvmeController;
// use crate::fault_injection::queue::SubmissionQueueFaultInjection;
use crate::fault_injection::FaultInjector;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use crate::spec;
use crate::workers::IoQueueEntrySizes;
use futures::FutureExt;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
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
    #[inspect(skip)]
    admin_sq_fault: Box<dyn FaultInjector + Send + Sync>,
}

#[derive(Inspect)]
pub(crate) struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueue,
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
    pub sq_doorbell_addr: u16, // The address of the submission queue doorbell in the device's BAR0.
}

impl AsyncRun<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), Cancelled> {
        loop {
            let event = stop.until_stopped(self.next_event(state)).await?;
            if let Err(err) = self.process_event(state, event).await {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "admin fault injection queue failure"
                );
                break;
            }
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

    async fn process_event(
        &mut self,
        state: &mut AdminStateFaultInjection,
        event: Result<Event, QueueError>,
    ) -> Result<(), QueueError> {
        let event = event?;
        match event {
            Event::Command(command_result) => {
                let command = command_result?;
                let opcode = spec::AdminOpcode(command.cdw0.opcode());

                let data = state.admin_sq.sqhd() as u32;
                let mut inner_controller = self.config.controller.lock();
                let data = u32::to_ne_bytes(data);

                // Write to doorbell register address
                let _ = inner_controller.write_bar0(self.config.sq_doorbell_addr, &data);
            }
        }
        Ok(())
    }
}

impl AdminHandlerFaultInjection {
    pub fn new(
        driver: VmTaskDriver,
        config: AdminConfigFaultInjection,
        admin_sq_fault: Box<dyn FaultInjector + Send + Sync>,
    ) -> Self {
        Self {
            driver,
            config,
            admin_sq_fault,
        }
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
            admin_sq: SubmissionQueue::new(handler.config.doorbells[0].clone(), asq, asqs, None),
        }
    }

    /// Returns the doorbells that need to be fault injected to. In this case it is just the admin submission queue
    /// TODO: Can be extended in the future to return a Vec<u16> of doorbell indices.
    pub fn get_intercept_doorbell(&self) -> u16 {
        0
    }
}
