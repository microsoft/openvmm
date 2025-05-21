// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! State unit for managing the VM partition and associated virtual processors.

mod debug;
mod vp_set;

pub use vp_set::Halt;
pub use vp_set::RequestYield;
pub use vp_set::RunCancelled;
pub use vp_set::RunnerCanceller;
pub use vp_set::VpRunner;
pub use vp_set::block_on_vp;

use self::vp_set::RegisterSetError;
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::Receiver;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use state_unit::NameInUse;
use state_unit::SpawnedUnit;
use state_unit::StateRequest;
use state_unit::StateUnit;
use state_unit::UnitBuilder;
use state_unit::UnitHandle;
use std::sync::Arc;
use thiserror::Error;
use virt::InitialRegs;
use virt::PageVisibility;
use vm_topology::processor::ProcessorTopology;
use vmcore::save_restore::ProtobufSaveRestore;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmm_core_defs::HaltReason;
use vp_set::VpSet;

/// The control point for managing a partition unit.
pub struct PartitionUnit {
    handle: SpawnedUnit<PartitionUnitRunner>,
    req_send: mesh::Sender<PartitionRequest>,
}

/// Trait with the minimal methods needed to run the partition.
#[async_trait]
pub trait VmPartition: 'static + Send + Sync + InspectMut + ProtobufSaveRestore {
    /// Resets the partition.
    fn reset(&mut self) -> anyhow::Result<()>;

    /// Scrubs the VTL state for a partition.
    fn scrub_vtl(&mut self, vtl: Vtl) -> anyhow::Result<()>;

    /// Accepts pages on behalf of the loader.
    fn accept_initial_pages(
        &mut self,
        pages: Vec<(MemoryRange, PageVisibility)>,
    ) -> anyhow::Result<()>;
}

/// An object to run the VM partition state unit.
struct PartitionUnitRunner {
    partition: Box<dyn VmPartition>,
    vp_set: VpSet,
    unit_started: bool,
    vp_stop_count: usize,
    needs_reset: bool,
    halt_reason: Option<HaltReason>,
    halt_request_recv: Receiver<InternalHaltReason>,
    client_notify_send: mesh::Sender<HaltReason>,
    req_recv: Receiver<PartitionRequest>,
    topology: ProcessorTopology,
    initial_regs: Option<Arc<InitialRegs>>,

    #[cfg(feature = "gdb")]
    debugger_state: debug::DebuggerState,
}

impl InspectMut for PartitionUnitRunner {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field(
                "power_state",
                self.halt_reason.as_ref().map_or("running", |_| "halted"),
            )
            .merge(&self.halt_reason)
            .merge(&self.vp_set)
            .field_mut_with("clear_halt", |clear| {
                // Clear halt if "true" is specified.
                if let Some(clear) = clear {
                    match clear.parse::<bool>() {
                        Ok(x) => {
                            if x {
                                self.clear_halt();
                            }
                            Ok(x)
                        }
                        Err(err) => Err(err),
                    }
                } else {
                    Ok(false)
                }
            })
            .field("topology", &self.topology)
            .merge(&mut self.partition);
    }
}

enum PartitionRequest {
    ClearHalt(Rpc<(), bool>), // TODO: remove this, and use DebugRequest::Resume
    SetInitialRegs(Rpc<(Vtl, Arc<InitialRegs>), Result<(), InitialRegError>>),
    SetInitialPageVisibility(
        Rpc<Vec<(MemoryRange, PageVisibility)>, Result<(), InitialVisibilityError>>,
    ),
    StopVps(Rpc<(), ()>),
    StartVps,
}

pub struct PartitionUnitParams<'a> {
    pub vtl_guest_memory: [Option<&'a GuestMemory>; 3],
    pub processor_topology: &'a ProcessorTopology,
    /// Tracks the halt state of VPs.
    pub halt_vps: Arc<Halt>,
    /// The receiver returned from `Halt::new()`.
    pub halt_request_recv: HaltReasonReceiver,
    /// Notified when the partition has been halted (due to a triple fault or
    /// other reason).
    pub client_notify_send: mesh::Sender<HaltReason>,
    pub debugger_rpc: Option<Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
}

/// The halt reason receiver to pass to put in [`PartitionUnitParams`].
pub struct HaltReasonReceiver(Receiver<InternalHaltReason>);

enum InternalHaltReason {
    Halt(HaltReason),
    ReplayMtrrs,
}

/// Error returned by [`PartitionUnit::new()`].
#[derive(Debug, Error)]
pub enum Error {
    #[error("debugging is not supported in this build")]
    DebuggingNotSupported,
    #[error(transparent)]
    NameInUse(NameInUse),
    #[error("missing guest memory required for gdb support")]
    MissingGuestMemory,
}

/// Error returned by [`PartitionUnit::set_initial_regs()`].
#[derive(Debug, Error)]
pub enum InitialRegError {
    #[error("failed to set registers")]
    RegisterSet(#[source] RegisterSetError),
    #[error("failed to scrub VTL state")]
    ScrubVtl(#[source] anyhow::Error),
}

/// Error returned by [`PartitionUnit::set_initial_page_visibility()`].
#[derive(Debug, Error)]
pub enum InitialVisibilityError {
    #[error("failed to set initial page acceptance")]
    PageAcceptance(#[source] anyhow::Error),
}

impl PartitionUnit {
    /// Creates a new VM partition state unit.
    ///
    /// The caller is responsible for launching a thread for each VP and running
    /// the VP using the returned [`VpRunner`]s.
    pub fn new(
        spawner: impl Spawn,
        builder: UnitBuilder<'_>,
        partition: impl VmPartition,
        params: PartitionUnitParams<'_>,
    ) -> Result<(Self, Vec<VpRunner>), Error> {
        #[cfg(not(feature = "gdb"))]
        if params.debugger_rpc.is_some() {
            return Err(Error::DebuggingNotSupported);
        }

        let mut vp_set = VpSet::new(params.vtl_guest_memory.map(|m| m.cloned()), params.halt_vps);
        let vps = params
            .processor_topology
            .vps_arch()
            .map(|vp| vp_set.add(vp))
            .collect();

        let (req_send, req_recv) = mesh::channel();

        let mut runner = PartitionUnitRunner {
            partition: Box::new(partition),
            vp_set,
            unit_started: false,
            vp_stop_count: 0,
            needs_reset: false,
            halt_reason: None,
            halt_request_recv: params.halt_request_recv.0,
            client_notify_send: params.client_notify_send,
            req_recv,
            topology: params.processor_topology.clone(),
            initial_regs: None,
            #[cfg(feature = "gdb")]
            debugger_state: debug::DebuggerState::new(
                params.vtl_guest_memory[0]
                    .ok_or(Error::MissingGuestMemory)?
                    .clone(),
                params.debugger_rpc,
            ),
        };

        let handle = builder
            .spawn(spawner, async |recv| {
                runner.run(recv).await;
                runner
            })
            .unwrap();

        Ok((Self { handle, req_send }, vps))
    }

    /// Gets the handle for the partition unit.
    pub fn unit_handle(&self) -> &UnitHandle {
        self.handle.handle()
    }

    /// Tears down the state unit, returning the `client_notify_send` sender
    /// passed to [`Self::new()`].
    pub async fn teardown(self) -> mesh::Sender<HaltReason> {
        let runner = self.handle.remove().await;
        runner.vp_set.teardown().await;
        runner.client_notify_send
    }

    /// Clears the current halt reason from the partition, resuming the VPs if
    /// they are stopped.
    pub async fn clear_halt(&mut self) -> bool {
        self.req_send
            .call(PartitionRequest::ClearHalt, ())
            .await
            .unwrap()
    }

    /// Temporarily stops the VPs, returning a guard that will resume them when
    /// dropped.
    pub async fn temporarily_stop_vps(&mut self) -> StopGuard {
        self.req_send
            .call(PartitionRequest::StopVps, ())
            .await
            .unwrap();

        StopGuard(self.req_send.clone())
    }

    /// Sets the register state for the VPs for initial boot.
    ///
    /// If the VM has been run before and has not been reset since it last ran,
    /// the target VTL will be scrubbed first so that the partition state is
    /// clean.
    pub async fn set_initial_regs(
        &mut self,
        vtl: Vtl,
        state: Arc<InitialRegs>,
    ) -> Result<(), InitialRegError> {
        self.req_send
            .call(PartitionRequest::SetInitialRegs, (vtl, state))
            .await
            .unwrap()
    }

    pub async fn set_initial_page_visibility(
        &mut self,
        vis: Vec<(MemoryRange, PageVisibility)>,
    ) -> Result<(), InitialVisibilityError> {
        self.req_send
            .call(PartitionRequest::SetInitialPageVisibility, vis)
            .await
            .unwrap()
    }
}

impl PartitionUnitRunner {
    /// Runs the VM partition, handling state change requests from `recv`.
    async fn run(&mut self, mut recv: Receiver<StateRequest>) {
        loop {
            enum Event {
                State(Option<StateRequest>),
                Halt(InternalHaltReason),
                Request(PartitionRequest),
                #[cfg(feature = "gdb")]
                Debug(vmm_core_defs::debug_rpc::DebugRequest),
            }

            #[cfg(feature = "gdb")]
            let debug = self.debugger_state.wait_rpc();
            #[cfg(not(feature = "gdb"))]
            let debug = std::future::pending();

            let event = futures::select! {  // merge semantics
                request = recv.next() => Event::State(request),
                request = self.halt_request_recv.select_next_some() => Event::Halt(request),
                request = self.req_recv.select_next_some() => Event::Request(request),
                request = debug.fuse() => {
                    #[cfg(feature = "gdb")]
                    {
                        Event::Debug(request)
                    }
                    #[cfg(not(feature = "gdb"))]
                    {
                        let _: std::convert::Infallible = request;
                        unreachable!()
                    }
                }
            };

            match event {
                Event::State(request) => {
                    if let Some(request) = request {
                        request.apply(self).await;
                    } else {
                        break;
                    }
                }
                Event::Halt(reason) => {
                    // Wait for the VPs to stop before reporting this anywhere.
                    // This is generally good behavior, but it is especially
                    // necessary because Self::clear_halt() will call
                    // VpSet::clear_halt(), which relies on the VPs being
                    // affirmatively stopped.
                    self.vp_set.stop().await;
                    self.handle_halt(reason).await;
                }
                Event::Request(request) => match request {
                    PartitionRequest::ClearHalt(rpc) => rpc.handle_sync(|()| self.clear_halt()),
                    PartitionRequest::SetInitialRegs(rpc) => {
                        rpc.handle(async |(vtl, state)| self.set_initial_regs(vtl, state).await)
                            .await
                    }
                    PartitionRequest::SetInitialPageVisibility(rpc) => {
                        rpc.handle(async |vis| self.set_initial_page_visibility(vis).await)
                            .await
                    }
                    PartitionRequest::StopVps(rpc) => {
                        rpc.handle(async |()| {
                            self.vp_set.stop().await;
                            self.vp_stop_count += 1;
                        })
                        .await
                    }
                    PartitionRequest::StartVps => {
                        self.vp_stop_count -= 1;
                        self.try_start();
                    }
                },
                #[cfg(feature = "gdb")]
                Event::Debug(request) => {
                    self.handle_gdb(request).await;
                }
            }
        }

        if self.unit_started {
            self.vp_set.stop().await;
        }
    }

    async fn handle_halt(&mut self, reason: InternalHaltReason) {
        match reason {
            InternalHaltReason::Halt(reason) => {
                // Only report the first halt request per boot so that the
                // client does not have to deal with multiple halt reasons
                // due to race conditions.
                if self.halt_reason.is_none() {
                    self.halt_reason = Some(reason.clone());

                    // Report the halt to the debugger.
                    #[cfg(feature = "gdb")]
                    let reported = self.debugger_state.report_halt_to_debugger(&reason);
                    #[cfg(not(feature = "gdb"))]
                    let reported = false;

                    // If the debugger is not attached, then report the halt
                    // to the client.
                    if !reported {
                        self.client_notify_send.send(reason);
                    }
                } else {
                    // Clear this specific halt.
                    self.vp_set.clear_halt();
                }
            }
            InternalHaltReason::ReplayMtrrs => {
                if let Some(initial_regs) = self.initial_regs.clone() {
                    if let Err(err) = self
                        .vp_set
                        .set_initial_regs(
                            Vtl::Vtl0,
                            initial_regs,
                            vp_set::RegistersToSet::MtrrsOnly,
                        )
                        .await
                    {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed to replay mtrrs, guest may see inconsistent results"
                        );
                    }
                } else {
                    tracing::warn!("no initial mtrrs to replay");
                }
                self.vp_set.clear_halt();
                self.try_start();
            }
        }
    }

    /// Clears the halt and resumes the VPs if the partition is started. Returns
    /// `false` if VPs were not already halted.
    fn clear_halt(&mut self) -> bool {
        if self.halt_reason.is_some() {
            self.halt_reason = None;
            self.vp_set.clear_halt();
            self.try_start();
            true
        } else {
            false
        }
    }

    async fn set_initial_regs(
        &mut self,
        vtl: Vtl,
        state: Arc<InitialRegs>,
    ) -> Result<(), InitialRegError> {
        assert!(!self.unit_started || self.vp_stop_count > 0);

        // If this VM has been run before, then automatically scrub the target
        // VTL state.
        if self.needs_reset {
            self.partition
                .scrub_vtl(vtl)
                .map_err(InitialRegError::ScrubVtl)?;
            self.needs_reset = false;
        }

        self.vp_set
            .set_initial_regs(vtl, state.clone(), vp_set::RegistersToSet::All)
            .await
            .map_err(InitialRegError::RegisterSet)?;

        self.initial_regs = Some(state);
        Ok(())
    }

    async fn set_initial_page_visibility(
        &mut self,
        visibility: Vec<(MemoryRange, PageVisibility)>,
    ) -> Result<(), InitialVisibilityError> {
        assert!(!self.unit_started);

        self.partition
            .accept_initial_pages(visibility)
            .map_err(InitialVisibilityError::PageAcceptance)
    }

    fn try_start(&mut self) {
        if self.unit_started && self.halt_reason.is_none() && self.vp_stop_count == 0 {
            self.needs_reset = true;
            self.vp_set.start();
        }
    }
}

#[must_use = "when dropped, the VPs will be resumed"]
pub struct StopGuard(mesh::Sender<PartitionRequest>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.send(PartitionRequest::StartVps);
    }
}

impl StateUnit for PartitionUnitRunner {
    async fn start(&mut self) {
        self.unit_started = true;
        self.try_start();
    }

    async fn stop(&mut self) {
        self.vp_set.stop().await;
        self.unit_started = false;

        // Now that the VM is stopped, flush any guest-initiated
        // power state change that may have raced with this request.
        while let Ok(reason) = self.halt_request_recv.try_recv() {
            self.handle_halt(reason).await;
        }
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.partition.reset()?;
        self.clear_halt();
        self.needs_reset = false;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let state = self.save().await?;
        Ok(Some(SavedStateBlob::new(state)))
    }

    async fn restore(&mut self, buffer: SavedStateBlob) -> Result<(), RestoreError> {
        // TODO: restore halted state
        self.needs_reset = true;
        self.restore(buffer.parse()?).await?;
        Ok(())
    }
}

mod save_restore {
    use super::PartitionUnitRunner;
    use virt::VpIndex;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateBlob;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "partition")]
        pub struct Partition {
            #[mesh(1)]
            pub(super) partition: SavedStateBlob,
            #[mesh(2)]
            pub(super) vps: Vec<Vp>,
            // TODO: save halted state
        }

        #[derive(Protobuf)]
        #[mesh(package = "partition")]
        pub struct Vp {
            #[mesh(1)]
            pub vp_index: u32,
            #[mesh(2)]
            pub data: SavedStateBlob,
        }
    }

    impl PartitionUnitRunner {
        pub async fn save(&mut self) -> Result<state::Partition, SaveError> {
            let partition = self.partition.save()?;
            let vps = self.vp_set.save().await?;
            let vps = vps
                .into_iter()
                .map(|(vp_index, data)| state::Vp {
                    vp_index: vp_index.index(),
                    data,
                })
                .collect();

            Ok(state::Partition { partition, vps })
        }

        pub async fn restore(&mut self, state: state::Partition) -> Result<(), RestoreError> {
            let state::Partition { partition, vps } = state;
            self.partition.restore(partition)?;
            self.vp_set
                .restore(
                    vps.into_iter()
                        .map(|state::Vp { vp_index, data }| (VpIndex::new(vp_index), data)),
                )
                .await?;
            Ok(())
        }
    }
}
