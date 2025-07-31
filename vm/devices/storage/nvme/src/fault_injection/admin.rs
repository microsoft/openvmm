use crate::NvmeController;
// use crate::fault_injection::queue::SubmissionQueueFaultInjection;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
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
    Command(Result<spec::Command, QueueError>),
}

/// An admin handler shim layer for fault injection.
#[derive(Inspect)]
pub(crate) struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfigFaultInjection,
}

#[derive(Inspect)]
pub(crate) struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueue,
    pub admin_sq_gpa: u64, // The guest physical address of the admin submission queue.
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
    #[inspect(skip)]
    pub sq_fault_injector: Box<
        dyn Fn(
                VmTaskDriver,
                spec::Command,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<spec::Command>> + Send>>
            + Send
            + Sync,
    >,
}

impl AsyncRun<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), Cancelled> {
        loop {
            let curr_head = state.admin_sq.sqhd();
            let event = stop.until_stopped(self.next_event(state)).await?;
            if let Err(err) = self.process_event(state, event, curr_head).await {
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
        event_head: u16,
    ) -> Result<(), QueueError> {
        let event = event?;
        match event {
            Event::Command(command_result) => {
                let command = command_result?;
                let output_command =
                    (self.config.sq_fault_injector)(self.driver.clone(), command).await; // TODO: Is there a good way to avoid cloning the driver here?

                // Fault inject a changed Command
                if let Some(output_command) = output_command {
                    let gpa = state.admin_sq_gpa.wrapping_add(event_head as u64 * 64);

                    // let data: u64 = output_command;
                    self.config
                        .mem
                        .write_plain(gpa, &output_command)
                        .map_err(QueueError::Memory)?;
                }

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
            admin_sq: SubmissionQueue::new(handler.config.doorbells[0].clone(), asq, asqs, None),
            admin_sq_gpa: asq,
        }
    }

    /// Returns the doorbells that need to be fault injected to. In this case it is just the admin submission queue
    /// TODO: Can be extended in the future to return a Vec<u16> of doorbell indices.
    pub fn get_intercept_doorbell(&self) -> u16 {
        0
    }
}
