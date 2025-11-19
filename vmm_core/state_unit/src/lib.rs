// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM state machine unit handling.
//!
//! A state unit is a VM component (such as a device) that needs to react to
//! changes in the VM state machine. It needs to start, stop, reset, and
//! save/restore with the VM. (Save/restore is not really a state change but is
//! modeled as such since it must be synchronized with actual state changes.)
//!
//! This module contains types and functions for defining and manipulating state
//! units. It does this in three parts:
//!
//! 1. It defines an RPC enum [`StateRequest`] which is used to request that a
//!    state unit change state (start, stop, etc.). Each state unit must handle
//!    incoming state requests on a mesh receiver. This is the foundational
//!    type of this model.
//!
//! 2. It defines a type [`StateUnits`], which is a collection of mesh senders
//!    for sending `StateRequest`s. This is used to initiate and wait for state
//!    changes across all the units in the VMM, handling any required dependency
//!    ordering.
//!
//! 3. It defines a trait [`StateUnit`] that can be used to define handlers for
//!    each state request. This is an optional convenience; not all state units
//!    will have a type that implements this trait.
//!
//! This model allows for asynchronous, highly concurrent state changes, and it
//! works across process boundaries thanks to `mesh`.

#![forbid(unsafe_code)]

use futures::FutureExt;
use futures::StreamExt;
use futures::future::join_all;
use futures_concurrency::stream::Merge;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::MeshPayload;
use mesh::Receiver;
use mesh::Sender;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map;
use std::fmt::Debug;
use std::fmt::Display;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Instant;
use thiserror::Error;
use tracing::Instrument;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// A state change request.
#[derive(Debug, MeshPayload)]
pub enum StateRequest {
    /// Start asynchronous operations.
    Start(Rpc<(), ()>),

    /// Stop asynchronous operations.
    Stop(Rpc<(), ()>),

    /// Reset a stopped unit to initial state.
    Reset(FailableRpc<(), ()>),

    /// Save state of a stopped unit.
    Save(FailableRpc<(), Option<SavedStateBlob>>),

    /// Restore state of a stopped unit.
    Restore(FailableRpc<SavedStateBlob, ()>),

    /// Inspect state.
    Inspect(inspect::Deferred),
}

/// Trait implemented by an object that can act as a state unit.
///
/// Implementing this is optional, to be used with [`UnitBuilder::spawn`] or
/// [`StateRequest::apply`]; state units can also directly process incoming
/// [`StateRequest`]s.
#[expect(async_fn_in_trait)] // Don't need Send bounds
pub trait StateUnit: InspectMut {
    /// Start asynchronous processing.
    async fn start(&mut self);

    /// Stop asynchronous processing.
    async fn stop(&mut self);

    /// Reset to initial state.
    ///
    /// Must only be called while stopped.
    async fn reset(&mut self) -> anyhow::Result<()>;

    /// Save state.
    ///
    /// Must only be called while stopped.
    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError>;

    /// Restore state.
    ///
    /// Must only be called while stopped.
    async fn restore(&mut self, buffer: SavedStateBlob) -> Result<(), RestoreError>;
}

/// Runs a simple unit that only needs to respond to state requests.
pub async fn run_unit<T: StateUnit>(mut unit: T, mut recv: Receiver<StateRequest>) -> T {
    while let Some(req) = recv.next().await {
        req.apply(&mut unit).await;
    }
    unit
}

/// Runs a state unit that can handle inspect requests while there is an active
/// state transition.
pub async fn run_async_unit<T>(unit: T, mut recv: Receiver<StateRequest>) -> T
where
    for<'a> &'a T: StateUnit,
{
    while let Some(req) = recv.next().await {
        req.apply_with_concurrent_inspects(&mut &unit, &mut recv)
            .await;
    }
    unit
}

impl StateRequest {
    /// Runs this state request against `unit`, polling `recv` for incoming
    /// inspect requests and applying them while any state transition is in
    /// flight.
    ///
    /// For this to work, your state unit `T` should implement [`StateUnit`] for
    /// `&'_ T`.
    ///
    /// Panics if a state transition arrives on `recv` while this one is being
    /// processed. This would indicate a contract violation with [`StateUnits`].
    pub async fn apply_with_concurrent_inspects<'a, T>(
        self,
        unit: &mut &'a T,
        recv: &mut Receiver<StateRequest>,
    ) where
        &'a T: StateUnit,
    {
        match self {
            StateRequest::Inspect(_) => {
                // This request has no response and completes synchronously,
                // so don't wait for concurrent requests.
                self.apply(unit).await;
            }

            StateRequest::Start(_)
            | StateRequest::Stop(_)
            | StateRequest::Reset(_)
            | StateRequest::Save(_)
            | StateRequest::Restore(_) => {
                // Handle for concurrent inspect requests.
                enum Event {
                    OpDone,
                    Req(StateRequest),
                }
                let mut op_unit = *unit;
                let op = pin!(self.apply(&mut op_unit).into_stream());
                let mut stream = (op.map(|()| Event::OpDone), recv.map(Event::Req)).merge();
                while let Some(Event::Req(next_req)) = stream.next().await {
                    match next_req {
                        StateRequest::Inspect(req) => req.inspect(&mut *unit),
                        _ => panic!(
                            "unexpected state transition {next_req:?} during state transition"
                        ),
                    }
                }
            }
        }
    }

    /// Runs this state request against `unit`.
    pub async fn apply(self, unit: &mut impl StateUnit) {
        match self {
            StateRequest::Start(rpc) => rpc.handle(async |()| unit.start().await).await,
            StateRequest::Stop(rpc) => rpc.handle(async |()| unit.stop().await).await,
            StateRequest::Reset(rpc) => rpc.handle_failable(async |()| unit.reset().await).await,
            StateRequest::Save(rpc) => rpc.handle_failable(async |()| unit.save().await).await,
            StateRequest::Restore(rpc) => {
                rpc.handle_failable(async |buffer| unit.restore(buffer).await)
                    .await
            }
            StateRequest::Inspect(req) => req.inspect(unit),
        }
    }
}

/// A set of state units.
#[derive(Debug)]
pub struct StateUnits {
    inner: Arc<Mutex<Inner>>,
    running: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Inspect)]
enum State {
    Stopped,
    Starting,
    Running,
    Stopping,
    Resetting,
    Saving,
    Restoring,
}

#[derive(Debug)]
struct Inner {
    next_id: u64,
    units: BTreeMap<u64, Unit>,
    names: HashMap<Arc<str>, u64>,
}

#[derive(Debug)]
struct Unit {
    name: Arc<str>,
    send: Sender<StateRequest>,
    dependencies: Vec<u64>,
    dependents: Vec<u64>,
    state: State,
}

/// An error returned when a state unit name is already in use.
#[derive(Debug, Error)]
#[error("state unit name {0} is in use")]
pub struct NameInUse(Arc<str>);

#[derive(Debug, Error)]
#[error("critical unit communication failure: {name}")]
struct UnitRecvError {
    name: Arc<str>,
    #[source]
    source: RpcError,
}

#[derive(Debug, Clone)]
struct UnitId {
    name: Arc<str>,
    id: u64,
}

/// A handle returned by [`StateUnits::add`], used to remove the state unit.
#[must_use]
#[derive(Debug)]
pub struct UnitHandle {
    id: UnitId,
    inner: Option<Weak<Mutex<Inner>>>,
}

impl Drop for UnitHandle {
    fn drop(&mut self) {
        self.remove_if();
    }
}

impl UnitHandle {
    /// Remove the state unit.
    pub fn remove(mut self) {
        self.remove_if();
    }

    /// Detach this handle, leaving the unit in place indefinitely.
    pub fn detach(mut self) {
        self.inner = None;
    }

    fn remove_if(&mut self) {
        if let Some(inner) = self.inner.take().and_then(|inner| inner.upgrade()) {
            let mut inner = inner.lock();
            inner.units.remove(&self.id.id).expect("unit exists");
            inner.names.remove(&self.id.name).expect("unit exists");
        }
    }
}

/// An object returned by [`StateUnits::inspector`] to inspect state units while
/// state transitions may be in flight.
pub struct StateUnitsInspector {
    inner: Weak<Mutex<Inner>>,
}

impl Inspect for StateUnits {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.inner.lock().inspect(req);
    }
}

impl Inspect for StateUnitsInspector {
    fn inspect(&self, req: inspect::Request<'_>) {
        if let Some(inner) = self.inner.upgrade() {
            inner.lock().inspect(req);
        }
    }
}

impl Inspect for Inner {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for unit in self.units.values() {
            resp.child(unit.name.as_ref(), |req| {
                let mut resp = req.respond();
                if !unit.dependencies.is_empty() {
                    resp.field_with("dependencies", || {
                        unit.dependencies
                            .iter()
                            .map(|id| self.units[id].name.as_ref())
                            .collect::<Vec<_>>()
                            .join(",")
                    });
                }
                if !unit.dependents.is_empty() {
                    resp.field_with("dependents", || {
                        unit.dependents
                            .iter()
                            .map(|id| self.units[id].name.as_ref())
                            .collect::<Vec<_>>()
                            .join(",")
                    });
                }
                resp.field("unit_state", unit.state)
                    .merge(&inspect::send(&unit.send, StateRequest::Inspect));
            });
        }
    }
}

/// The saved state for an individual unit.
#[derive(Protobuf)]
#[mesh(package = "state_unit")]
pub struct SavedStateUnit {
    /// The name of the state unit.
    #[mesh(1)]
    pub name: String,
    /// The opaque saved state blob.
    #[mesh(2)]
    pub state: SavedStateBlob,
}

/// An error from a state transition.
#[derive(Debug, Error)]
#[error("{op} failed")]
pub struct StateTransitionError {
    op: &'static str,
    #[source]
    errors: UnitErrorSet,
}

fn extract<T, E: Into<anyhow::Error>, U>(
    op: &'static str,
    iter: impl IntoIterator<Item = (Arc<str>, Result<T, E>)>,
    mut f: impl FnMut(Arc<str>, T) -> Option<U>,
) -> Result<Vec<U>, StateTransitionError> {
    let mut result = Vec::new();
    let mut errors = Vec::new();
    for (name, item) in iter {
        match item {
            Ok(t) => {
                if let Some(u) = f(name, t) {
                    result.push(u);
                }
            }
            Err(err) => errors.push((name, err.into())),
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(StateTransitionError {
            op,
            errors: UnitErrorSet(errors),
        })
    }
}

fn check<E: Into<anyhow::Error>>(
    op: &'static str,
    iter: impl IntoIterator<Item = (Arc<str>, Result<(), E>)>,
) -> Result<(), StateTransitionError> {
    extract(op, iter, |_, _| Some(()))?;
    Ok(())
}

#[derive(Debug)]
struct UnitErrorSet(Vec<(Arc<str>, anyhow::Error)>);

impl Display for UnitErrorSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (name, err) in &self.0 {
            map.entry(&format_args!("{}", name), &format_args!("{:#}", err));
        }
        map.finish()
    }
}

impl std::error::Error for UnitErrorSet {}

impl StateUnits {
    /// Creates a new instance with no initial units.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_id: 0,
                units: BTreeMap::new(),
                names: HashMap::new(),
            })),
            running: false,
        }
    }

    /// Returns an inspector that can be used to inspect the state units while
    /// state transitions are in process.
    pub fn inspector(&self) -> StateUnitsInspector {
        StateUnitsInspector {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Save and restore will use `name` as the save ID, so this forms part of
    /// the saved state.
    ///
    /// Note that the added unit will not be running after it is built/spawned,
    /// even if the other units are running. Call
    /// [`StateUnits::start_stopped_units`] when finished adding units.
    pub fn add(&self, name: impl Into<Arc<str>>) -> UnitBuilder<'_> {
        UnitBuilder {
            units: self,
            name: name.into(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
        }
    }

    /// Check if state units are currently running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Starts any units that are individually stopped, because they were added
    /// via [`StateUnits::add`] while the VM was running.
    ///
    /// Does nothing if all units are stopped, via [`StateUnits::stop`].
    pub async fn start_stopped_units(&mut self) {
        if self.is_running() {
            self.start().await;
        }
    }

    /// Starts all the state units.
    pub async fn start(&mut self) {
        self.run_op(
            "start",
            None,
            State::Stopped,
            State::Starting,
            State::Running,
            StateRequest::Start,
            |_, _| Some(()),
            |unit| &unit.dependencies,
        )
        .await;
        self.running = true;
    }

    /// Stops all the state units.
    pub async fn stop(&mut self) {
        assert!(self.running);
        // Stop units in reverse dependency order so that a dependency is not
        // stopped before its dependant.
        self.run_op(
            "stop",
            None,
            State::Running,
            State::Stopping,
            State::Stopped,
            StateRequest::Stop,
            |_, _| Some(()),
            |unit| &unit.dependents,
        )
        .await;
        self.running = false;
    }

    /// Resets all the state units.
    ///
    /// Panics if running.
    pub async fn reset(&mut self) -> Result<(), StateTransitionError> {
        assert!(!self.running);
        // Reset in dependency order so that dependants observe their
        // dependencies' reset state.
        let r = self
            .run_op(
                "reset",
                None,
                State::Stopped,
                State::Resetting,
                State::Stopped,
                StateRequest::Reset,
                |_, _| Some(()),
                |unit| &unit.dependencies,
            )
            .await;

        check("reset", r)?;
        Ok(())
    }

    /// Saves all the state units.
    ///
    /// Panics if running.
    pub async fn save(&mut self) -> Result<Vec<SavedStateUnit>, StateTransitionError> {
        assert!(!self.running);
        // Save can occur in any order since it will not observably mutate
        // state.
        let r = self
            .run_op(
                "save",
                None,
                State::Stopped,
                State::Saving,
                State::Stopped,
                StateRequest::Save,
                |_, _| Some(()),
                |_| &[],
            )
            .await;

        let states = extract("save", r, |name, state| {
            state.map(|state| SavedStateUnit {
                name: name.to_string(),
                state,
            })
        })?;

        Ok(states)
    }

    /// Restores all the state units.
    ///
    /// Panics if running.
    pub async fn restore(
        &mut self,
        states: Vec<SavedStateUnit>,
    ) -> Result<(), StateTransitionError> {
        assert!(!self.running);

        #[derive(Debug, Error)]
        enum RestoreUnitError {
            #[error("unknown unit name")]
            Unknown,
            #[error("duplicate unit name")]
            Duplicate,
        }

        let mut states_by_id = HashMap::new();
        let mut r = Vec::new();
        {
            let inner = self.inner.lock();
            for state in states {
                match inner.names.get_key_value(state.name.as_str()) {
                    Some((name, &id)) => {
                        if states_by_id
                            .insert(id, (name.clone(), state.state))
                            .is_some()
                        {
                            r.push((name.clone(), Err(RestoreUnitError::Duplicate)));
                        }
                    }
                    None => {
                        r.push((state.name.into(), Err(RestoreUnitError::Unknown)));
                    }
                }
            }
        }

        check("restore", r)?;

        let r = self
            .run_op(
                "restore",
                None,
                State::Stopped,
                State::Restoring,
                State::Stopped,
                StateRequest::Restore,
                |id, _| states_by_id.remove(&id).map(|(_, blob)| blob),
                |unit| &unit.dependencies,
            )
            .await;

        // Make sure all the saved state was consumed. This could hit if a unit
        // was removed concurrently with the restore.
        check(
            "restore",
            states_by_id
                .into_iter()
                .map(|(_, (name, _))| (name, Err(RestoreUnitError::Unknown))),
        )?;

        check("restore", r)?;

        Ok(())
    }

    /// Runs a state change operation on a set of units.
    ///
    /// `op` gives the name of the operation for tracing and error reporting
    /// purposes.
    ///
    /// `unit_ids` is the set of the units whose state should be changed. If
    /// `unit_ids` is `None`, all units change states.
    ///
    /// The old state for each unit must be `old_state`. During the operation,
    /// the unit is temporarily places in `interim_state`. When complete, the
    /// unit is placed in `new_state`.
    ///
    /// Each unit waits for its dependencies to complete their state change
    /// operation before proceeding with their own state change. The
    /// dependencies list is computed for a unit by calling `deps`.
    ///
    /// To perform the state change, the unit is sent a request generated using
    /// `request`, with input generated by `input`. If `input` returns `None`,
    /// then communication with the unit is skipped, but the unit still
    /// transitions through the interim and into the new state, and its
    /// dependencies are still waited on by its dependents.
    async fn run_op<I: 'static, R: 'static + Send>(
        &self,
        op: &str,
        unit_ids: Option<&[u64]>,
        old_state: State,
        interim_state: State,
        new_state: State,
        request: impl Copy + FnOnce(Rpc<I, R>) -> StateRequest,
        mut input: impl FnMut(u64, &Unit) -> Option<I>,
        mut deps: impl FnMut(&Unit) -> &[u64],
    ) -> Vec<(Arc<str>, R)> {
        let mut done = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(unit_ids);
            for (&id, unit) in inner
                .units
                .iter_mut()
                .filter(|(id, _)| ready_set.0.contains_key(id))
            {
                if unit.state != old_state {
                    assert_eq!(
                        unit.state, new_state,
                        "unit {} in {:?} state, should be {:?} or {:?}",
                        unit.name, unit.state, old_state, new_state
                    );
                    ready_set.done(id, true);
                } else {
                    let name = unit.name.clone();
                    let input = (input)(id, unit);
                    let ready_set = ready_set.clone();
                    let deps = deps(unit).to_vec();
                    let fut = state_change(name.clone(), unit, request, input);
                    let recv = async move {
                        ready_set.wait(op, id, &deps).await;
                        let r = fut.await;
                        ready_set.done(id, true);
                        (name, id, r)
                    };
                    done.push(recv);
                    unit.state = interim_state;
                }
            }
        }

        let results = async {
            let start = Instant::now();
            let results = join_all(done).await;
            tracing::info!(duration = ?Instant::now() - start, "state change complete");
            results
        }
        .instrument(tracing::info_span!("state_change", operation = op))
        .await;

        let mut inner = self.inner.lock();
        let r = results
            .into_iter()
            .filter_map(|(name, id, r)| {
                match r {
                    Ok(Some(r)) => Some((name, r)),
                    Ok(None) => None,
                    Err(err) => {
                        // If the unit was removed during the operation, then
                        // ignore the failure. Otherwise, panic because unit
                        // failure is not recoverable. FUTURE: reconsider this
                        // position.
                        if inner.units.contains_key(&id) {
                            panic!("{:?}", err);
                        }
                        None
                    }
                }
            })
            .collect();
        for (_, unit) in inner
            .units
            .iter_mut()
            .filter(|(id, _)| ready_set.0.contains_key(id))
        {
            if unit.state == interim_state {
                unit.state = new_state;
            } else {
                assert_eq!(
                    unit.state, new_state,
                    "unit {} in {:?} state, should be {:?} or {:?}",
                    unit.name, unit.state, interim_state, new_state
                );
            }
        }
        r
    }
}

impl Inner {
    fn ready_set(&self, unit_ids: Option<&[u64]>) -> ReadySet {
        let map = |id, unit: &Unit| {
            (
                id,
                ReadyState {
                    name: unit.name.clone(),
                    ready: Arc::new(Ready::default()),
                },
            )
        };
        let units = if let Some(unit_ids) = unit_ids {
            unit_ids
                .iter()
                .map(|id| map(*id, &self.units[id]))
                .collect()
        } else {
            self.units.iter().map(|(id, unit)| map(*id, unit)).collect()
        };
        ReadySet(Arc::new(units))
    }
}

#[derive(Clone)]
struct ReadySet(Arc<BTreeMap<u64, ReadyState>>);

#[derive(Clone)]
struct ReadyState {
    name: Arc<str>,
    ready: Arc<Ready>,
}

impl ReadySet {
    async fn wait(&self, op: &str, id: u64, deps: &[u64]) -> bool {
        for dep in deps {
            if let Some(dep) = self.0.get(dep) {
                if !dep.ready.is_ready() {
                    tracing::debug!(
                        device = self.0[&id].name.as_ref(),
                        dependency = dep.name.as_ref(),
                        operation = op,
                        "waiting on dependency"
                    );
                }
                if !dep.ready.wait().await {
                    return false;
                }
            }
        }
        true
    }

    fn done(&self, id: u64, success: bool) {
        self.0[&id].ready.signal(success);
    }
}

/// Sends state change `request` to `unit` with `input`, wrapping the result
/// future with a span, and wrapping its error with something more informative.
///
/// `operation` and `name` are used in tracing and error construction.
fn state_change<I: 'static, R: 'static + Send, Req: FnOnce(Rpc<I, R>) -> StateRequest>(
    name: Arc<str>,
    unit: &Unit,
    request: Req,
    input: Option<I>,
) -> impl Future<Output = Result<Option<R>, UnitRecvError>> + use<I, R, Req> {
    let send = unit.send.clone();

    async move {
        let Some(input) = input else { return Ok(None) };
        let span = tracing::info_span!("device_state_change", device = name.as_ref());
        async move {
            let start = Instant::now();
            let r = send
                .call(request, input)
                .await
                .map_err(|err| UnitRecvError { name, source: err });
            tracing::debug!(duration = ?Instant::now() - start, "device state change complete");
            r.map(Some)
        }
        .instrument(span)
        .await
    }
}

/// A builder returned by [`StateUnits::add`].
#[derive(Debug)]
#[must_use]
pub struct UnitBuilder<'a> {
    units: &'a StateUnits,
    name: Arc<str>,
    dependencies: Vec<u64>,
    dependents: Vec<u64>,
}

impl UnitBuilder<'_> {
    /// Adds `handle` as a dependency of this new unit.
    ///
    /// Operations will be ordered to ensure that a dependency will stop after
    /// its dependants, and that it will reset or restore before its dependants.
    pub fn depends_on(mut self, handle: &UnitHandle) -> Self {
        self.dependencies.push(self.handle_id(handle));
        self
    }

    /// Adds this new unit as a dependency of `handle`.
    ///
    /// Operations will be ordered to ensure that a dependency will stop after
    /// its dependants, and that it will reset or restore before its dependants.
    pub fn dependency_of(mut self, handle: &UnitHandle) -> Self {
        self.dependents.push(self.handle_id(handle));
        self
    }

    fn handle_id(&self, handle: &UnitHandle) -> u64 {
        // Ensure this handle is associated with this set of state units.
        assert_eq!(
            Weak::as_ptr(handle.inner.as_ref().unwrap()),
            Arc::as_ptr(&self.units.inner)
        );
        handle.id.id
    }

    /// Adds a new state unit sending requests to `send`.
    pub fn build(mut self, send: Sender<StateRequest>) -> Result<UnitHandle, NameInUse> {
        let id = {
            let mut inner = self.units.inner.lock();
            let id = inner.next_id;
            let entry = match inner.names.entry(self.name.clone()) {
                hash_map::Entry::Occupied(_) => return Err(NameInUse(self.name)),
                hash_map::Entry::Vacant(e) => e,
            };
            entry.insert(id);

            // Dedup the dependencies and update the dependencies' lists of
            // dependents.
            self.dependencies.sort();
            self.dependencies.dedup();
            for &dep in &self.dependencies {
                inner.units.get_mut(&dep).unwrap().dependents.push(id);
            }

            // Dedup the depenedents and update the dependents' lists of
            // dependencies.
            self.dependents.sort();
            self.dependents.dedup();
            for &dep in &self.dependents {
                inner.units.get_mut(&dep).unwrap().dependencies.push(id);
            }
            inner.units.insert(
                id,
                Unit {
                    name: self.name.clone(),
                    send,
                    dependencies: self.dependencies,
                    dependents: self.dependents,
                    state: State::Stopped,
                },
            );
            let unit_id = UnitId {
                name: self.name,
                id,
            };
            inner.next_id += 1;
            unit_id
        };
        Ok(UnitHandle {
            id,
            inner: Some(Arc::downgrade(&self.units.inner)),
        })
    }

    /// Adds a unit as in [`Self::build`], then spawns a task for running the
    /// unit.
    ///
    /// The channel to receive state change requests is passed to `f`, which
    /// should return the future to evaluate to run the unit.
    #[track_caller]
    pub fn spawn<F, Fut>(
        self,
        spawner: impl Spawn,
        f: F,
    ) -> Result<SpawnedUnit<Fut::Output>, NameInUse>
    where
        F: FnOnce(Receiver<StateRequest>) -> Fut,
        Fut: 'static + Send + Future,
        Fut::Output: 'static + Send,
    {
        let (send, recv) = mesh::channel();
        let task_name = format!("unit-{}", self.name);
        let handle = self.build(send)?;
        let fut = (f)(recv);
        let task = spawner.spawn(task_name, fut);
        Ok(SpawnedUnit { task, handle })
    }
}

/// A handle to a spawned unit.
#[must_use]
pub struct SpawnedUnit<T> {
    handle: UnitHandle,
    task: Task<T>,
}

impl<T> Debug for SpawnedUnit<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedUnit")
            .field("handle", &self.handle)
            .field("task", &self.task)
            .finish()
    }
}

impl<T> SpawnedUnit<T> {
    /// Removes the unit and returns it.
    pub async fn remove(self) -> T {
        self.handle.remove();
        self.task.await
    }

    /// Gets the unit handle for use with methods like
    /// [`UnitBuilder::depends_on`].
    pub fn handle(&self) -> &UnitHandle {
        &self.handle
    }
}

#[derive(Default)]
struct Ready {
    state: AtomicU32,
    event: event_listener::Event,
}

impl Ready {
    /// Wakes everyone with `success`.
    fn signal(&self, success: bool) {
        self.state.store(success as u32 + 1, Ordering::Release);
        self.event.notify(usize::MAX);
    }

    fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) != 0
    }

    /// Waits for `signal` to be called and returns its `success` parameter.
    async fn wait(&self) -> bool {
        loop {
            let listener = self.event.listen();
            let state = self.state.load(Ordering::Acquire);
            if state != 0 {
                return state - 1 != 0;
            }
            listener.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StateUnit;
    use super::StateUnits;
    use crate::run_unit;
    use inspect::InspectMut;
    use mesh::payload::Protobuf;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Default)]
    struct TestUnit {
        value: Arc<AtomicBool>,
        dep: Option<Arc<AtomicBool>>,
        /// If we should support saved state or not.
        support_saved_state: bool,
    }

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "test")]
    struct SavedState(bool);

    impl StateUnit for TestUnit {
        async fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            if self.support_saved_state {
                let state = SavedState(self.value.load(Ordering::Relaxed));
                Ok(Some(SavedStateBlob::new(state)))
            } else {
                Ok(None)
            }
        }

        async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
            assert!(self.dep.as_ref().is_none_or(|v| v.load(Ordering::Relaxed)));

            if self.support_saved_state {
                let state: SavedState = state.parse()?;
                self.value.store(state.0, Ordering::Relaxed);
                Ok(())
            } else {
                Err(RestoreError::SavedStateNotSupported)
            }
        }
    }

    impl InspectMut for TestUnit {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    struct TestUnitSetDep {
        dep: Arc<AtomicBool>,
        driver: DefaultDriver,
    }

    impl StateUnit for TestUnitSetDep {
        async fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(Some(SavedStateBlob::new(SavedState(true))))
        }

        async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
            pal_async::timer::PolledTimer::new(&self.driver)
                .sleep(Duration::from_millis(100))
                .await;

            self.dep.store(true, Ordering::Relaxed);

            Ok(())
        }
    }

    impl InspectMut for TestUnitSetDep {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    #[async_test]
    async fn test_state_change(driver: DefaultDriver) {
        let mut units = StateUnits::new();

        let a_val = Arc::new(AtomicBool::new(true));

        let _a = units
            .add("a")
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnit {
                        value: a_val.clone(),
                        dep: None,
                        support_saved_state: true,
                    },
                    recv,
                )
            })
            .unwrap();
        let _b = units
            .add("b")
            .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
            .unwrap();
        units.start().await;

        let _c = units
            .add("c")
            .spawn(&driver, |recv| run_unit(TestUnit::default(), recv));
        units.stop().await;
        units.start().await;

        units.stop().await;

        let state = units.save().await.unwrap();

        a_val.store(false, Ordering::Relaxed);

        units.restore(state).await.unwrap();

        assert!(a_val.load(Ordering::Relaxed));
    }

    #[async_test]
    async fn test_dependencies(driver: DefaultDriver) {
        let mut units = StateUnits::new();

        let a_val = Arc::new(AtomicBool::new(true));

        let a = units
            .add("zzz")
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnit {
                        value: a_val.clone(),
                        dep: None,
                        support_saved_state: true,
                    },
                    recv,
                )
            })
            .unwrap();

        let _b = units
            .add("aaa")
            .depends_on(a.handle())
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnit {
                        dep: Some(a_val.clone()),
                        value: Default::default(),
                        support_saved_state: true,
                    },
                    recv,
                )
            })
            .unwrap();
        units.start().await;
        units.stop().await;

        let state = units.save().await.unwrap();

        a_val.store(false, Ordering::Relaxed);

        units.restore(state).await.unwrap();
    }

    #[async_test]
    async fn test_dep_no_saved_state(driver: DefaultDriver) {
        let mut units = StateUnits::new();

        let true_val = Arc::new(AtomicBool::new(true));
        let shared_val = Arc::new(AtomicBool::new(false));

        let a = units
            .add("a")
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnit {
                        value: true_val.clone(),
                        dep: Some(shared_val.clone()),
                        support_saved_state: true,
                    },
                    recv,
                )
            })
            .unwrap();

        // no saved state
        // Note that restore is never called for this unit.
        let b = units
            .add("b_no_saved_state")
            .dependency_of(a.handle())
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnit {
                        value: true_val.clone(),
                        dep: Some(shared_val.clone()),
                        support_saved_state: false,
                    },
                    recv,
                )
            })
            .unwrap();

        // A has a transitive dependency on C via B.
        let _c = units
            .add("c")
            .dependency_of(b.handle())
            .spawn(&driver, |recv| {
                run_unit(
                    TestUnitSetDep {
                        dep: shared_val,
                        driver: driver.clone(),
                    },
                    recv,
                )
            })
            .unwrap();

        let state = units.save().await.unwrap();

        units.restore(state).await.unwrap();
    }
}
