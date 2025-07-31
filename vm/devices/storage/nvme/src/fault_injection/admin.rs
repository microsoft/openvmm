use crate::NvmeController;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use crate::spec;
use futures::FutureExt;
use guestmem::GuestMemory;
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

/// An admin handler built for fault injection.
#[derive(Inspect)]
pub(crate) struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfigFaultInjection,
}

#[derive(Inspect)]
pub(crate) struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueue,
    pub admin_sq_gpa: u64,
}

#[derive(Inspect)]
pub(crate) struct AdminConfigFaultInjection {
    #[inspect(skip)]
    pub mem: GuestMemory,
    #[inspect(skip)]
    pub controller: Arc<Mutex<NvmeController>>,
    pub admin_sq_doorbell_addr: u16,
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
                    (self.config.sq_fault_injector)(self.driver.clone(), command).await;

                // Fault inject a changed Command
                if let Some(output_command) = output_command {
                    let gpa = state.admin_sq_gpa.wrapping_add(event_head as u64 * 64);

                    self.config
                        .mem
                        .write_plain(gpa, &output_command)
                        .map_err(QueueError::Memory)?;
                }

                let data = state.admin_sq.sqhd() as u32;
                let mut inner_controller = self.config.controller.lock();
                let data = u32::to_ne_bytes(data);

                // Write to doorbell register address
                let _ = inner_controller.write_bar0(self.config.admin_sq_doorbell_addr, &data);
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
    pub fn new(asq: u64, asqs: u16, admin_sq_doorbell: Arc<DoorbellRegister>) -> Self {
        Self {
            admin_sq: SubmissionQueue::new(admin_sq_doorbell, asq, asqs, None),
            admin_sq_gpa: asq,
        }
    }
}
