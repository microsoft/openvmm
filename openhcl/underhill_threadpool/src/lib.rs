// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]

//! The Underhill per-CPU thread pool used to run async tasks and IO.
//!
//! This is built on top of [`pal_uring`] and [`pal_async`].

#![forbid(unsafe_code)]

use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use loan_cell::LoanCell;
use pal::unix::affinity::CpuSet;
use pal_async::fd::FdReadyDriver;
use pal_async::task::Runnable;
use pal_async::task::Schedule;
use pal_async::task::Spawn;
use pal_async::task::SpawnLocal;
use pal_async::task::TaskMetadata;
use pal_async::timer::TimerDriver;
use pal_async::wait::WaitDriver;
use pal_uring::FdReady;
use pal_uring::FdWait;
use pal_uring::IdleControl;
use pal_uring::Initiate;
use pal_uring::IoInitiator;
use pal_uring::IoUringPool;
use pal_uring::PoolClient;
use pal_uring::Timer;
use parking_lot::Mutex;
use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;

/// Represents the internal state of an `AffinitizedThreadpool`.
#[derive(Debug, Inspect)]
struct AffinitizedThreadpoolState {
    #[inspect(iter_by_index)]
    drivers: Vec<ThreadpoolDriver>,
}

/// A pool of affinitized worker threads.
#[derive(Clone, Debug, Inspect)]
#[inspect(transparent)]
pub struct AffinitizedThreadpool {
    state: Arc<AffinitizedThreadpoolState>,
}

/// A builder for [`AffinitizedThreadpool`].
#[derive(Debug, Clone)]
pub struct ThreadpoolBuilder {
    max_bounded_workers: Option<u32>,
    max_unbounded_workers: Option<u32>,
    ring_size: u32,
}

impl ThreadpoolBuilder {
    /// Returns a new builder.
    pub fn new() -> Self {
        Self {
            max_bounded_workers: None,
            max_unbounded_workers: None,
            ring_size: 256,
        }
    }

    /// Sets the maximum number of bounded kernel workers for each worker ring,
    /// per NUMA node.
    ///
    /// This defaults in the kernel to `min(io_ring_size, cpu_count * 4)`.
    pub fn max_bounded_workers(&mut self, n: u32) -> &mut Self {
        self.max_bounded_workers = Some(n);
        self
    }

    /// Sets the maximum number of unbounded kernel workers for each worker
    /// ring, per NUMA node.
    ///
    /// This defaults to the process's `RLIMIT_NPROC` limit at time of
    /// threadpool creation.
    pub fn max_unbounded_workers(&mut self, n: u32) -> &mut Self {
        self.max_unbounded_workers = Some(n);
        self
    }

    /// Sets the IO ring size. Defaults to 256.
    pub fn ring_size(&mut self, ring_size: u32) -> &mut Self {
        assert_ne!(ring_size, 0);
        self.ring_size = ring_size;
        self
    }

    /// Builds the thread pool.
    pub fn build(&self) -> io::Result<AffinitizedThreadpool> {
        let proc_count = pal::unix::affinity::max_present_cpu()? + 1;

        let builder = Arc::new(self.clone());
        let mut drivers = Vec::with_capacity(proc_count as usize);
        drivers.extend((0..proc_count).map(|processor| ThreadpoolDriver {
            inner: Arc::new(ThreadpoolDriverInner {
                once: OnceLock::new(),
                cpu: processor,
                builder: builder.clone(),
                name: format!("threadpool-{}", processor).into(),
                affinity_set: false.into(),
                state: Mutex::new(ThreadpoolDriverState {
                    notifier: None,
                    affinity: AffinityState::Waiting(Vec::new()),
                    spawned: false,
                }),
            }),
        }));

        let state = Arc::new(AffinitizedThreadpoolState { drivers });

        Ok(AffinitizedThreadpool { state })
    }

    // Spawn a pool on the specified CPU.
    //
    // If the specified CPU is present but not online, spawns a thread with
    // affinity set to all processors that are in the same package, if possible.
    //
    // Note that this sets affinity of the current thread and does not revert
    // it. Call this from a temporary thread to avoid permanently changing the
    // affinity of the current thread.
    fn spawn_pool(&self, cpu: u32, driver: ThreadpoolDriver) -> io::Result<PoolClient> {
        tracing::debug!(cpu, "starting threadpool thread");

        let online = is_cpu_online(cpu)?;
        let mut affinity = CpuSet::new();
        if online {
            affinity.set(cpu);
        } else {
            // The CPU is not online. Set the affinity to match the package.
            //
            // TODO: figure out how to do this (maybe pass in
            // ProcessorTopology)--the sysfs topology directory does not exist
            // for offline CPUs. For now, just allow all CPUs.
            let online_cpus = fs_err::read_to_string("/sys/devices/system/cpu/online")?;
            affinity
                .set_mask_list(&online_cpus)
                .map_err(io::Error::other)?;
        }

        // Set the current thread's affinity so that allocations for the worker
        // thread are performed in the correct node.
        let affinity_ok = match pal::unix::affinity::set_current_thread_affinity(&affinity) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::InvalidInput && !online => {
                // None of the CPUs in the package are online. That's not ideal,
                // because the thread will probably get allocated with the wrong node,
                // but it's recoverable.
                tracing::warn!(
                    CVM_ALLOWED,
                    cpu,
                    error = &err as &dyn std::error::Error,
                    "could not set package affinity for thread pool thread"
                );
                false
            }
            Err(err) => return Err(err),
        };

        let this = self.clone();
        let (send, recv) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tp".to_owned())
            .spawn(move || {
                // Create the pool and report back the result. This must be done
                // on the thread so that the io-uring task context gets created.
                // If we create this back on the initiating thread, then the
                // task context gets created and then destroyed, and subsequent
                // calls to update the affinity fail until the task context gets
                // recreated (next time an IO is issued).
                //
                // FUTURE: take advantage of the per-thread task context and
                // pre-register the ring via IORING_REGISTER_RING_FDS.
                let pool = match this
                    .make_ring(driver.inner.name.clone(), affinity_ok.then_some(&affinity))
                {
                    Ok(pool) => pool,
                    Err(err) => {
                        send.send(Err(err)).ok();
                        return;
                    }
                };

                let driver = driver;
                let notifier = {
                    let mut state = driver.inner.state.lock();
                    state.spawned = true;
                    if online {
                        // There cannot be any waiters yet since they can only
                        // be registered from the current thread.
                        driver.inner.affinity_set.store(true, Relaxed);
                        state.affinity = AffinityState::Set;
                    }
                    state.notifier.take()
                };

                send.send(Ok(pool.client().clone())).ok();

                // Store the current thread's driver so that spawned tasks can
                // find it via `Thread::current()`. Do this via a loan instead
                // of storing it directly in TLS to avoid the overhead of
                // registering a destructor.
                CURRENT_THREAD_DRIVER.with(|current| {
                    current.lend(&driver, || {
                        if let Some(notifier) = notifier {
                            (notifier.0)(true);
                        }
                        pool.run()
                    });
                });
            })?;

        // Wait for the pool to be initialized.
        recv.recv().unwrap().inspect_err(|_| {
            // Wait for the child thread to exit to bound resource use.
            thread.join().unwrap();
        })
    }

    fn make_ring(&self, name: Arc<str>, affinity: Option<&CpuSet>) -> io::Result<IoUringPool> {
        let pool = IoUringPool::new(name, self.ring_size)?;
        let client = pool.client();
        client.set_iowq_max_workers(self.max_bounded_workers, self.max_unbounded_workers)?;
        if let Some(affinity) = affinity {
            client.set_iowq_affinity(affinity)?
        }
        Ok(pool)
    }
}

/// Returns whether the specified CPU is online.
pub fn is_cpu_online(cpu: u32) -> io::Result<bool> {
    // Depending at the very minimum on whether the kernel has been built with
    // `CONFIG_HOTPLUG_CPU` or not, the individual `online` pseudo-files will be
    // present or absent.
    //
    // The other factors at play are the firmware-reported system properties and
    // the `cpu_ops` structures defined for the platform. All these lead ultimately
    // to setting the `hotpluggable` property on the cpu device in the kernel.
    // If that property is set, the `online` file will be present for the given CPU.
    //
    // If that file is absent for the CPU in question, that means it is online, and
    // due to various factors (e.g. BSP on x86_64, missing `cpu_die` handler, etc)
    // the CPU is not allowed to be offlined.
    //
    // The well-established cross-platform tools (e.g. `perf`) in the kernel repo
    // rely on the same: if the `online` file is missing, assume the CPU is online
    // provided the CPU "home" directory is present (although they don't have
    // comments like this one :)).

    let cpu_sysfs_home = format!("/sys/devices/system/cpu/cpu{cpu}");
    let cpu_sysfs_home = std::path::Path::new(cpu_sysfs_home.as_str());
    let online = cpu_sysfs_home.join("online");
    match fs_err::read_to_string(online) {
        Ok(s) => Ok(s.trim() == "1"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(cpu_sysfs_home.exists()),
        Err(err) => Err(err),
    }
}

/// Sets the specified CPU online, if it is not already online.
pub fn set_cpu_online(cpu: u32) -> io::Result<()> {
    let online = format!("/sys/devices/system/cpu/cpu{cpu}/online");
    match fs_err::read_to_string(&online) {
        Ok(s) if s.trim() == "0" => {
            fs_err::write(&online, "1")?;
        }
        Ok(_) => {
            // Already online.
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // The file doesn't exist, so the processor is always online.
        }
        Err(err) => return Err(err),
    }
    Ok(())
}

impl AffinitizedThreadpool {
    /// Creates a new threadpool with the specified ring size.
    pub fn new(io_ring_size: u32) -> io::Result<Self> {
        ThreadpoolBuilder::new().ring_size(io_ring_size).build()
    }

    /// Returns an object that can be used to submit IOs or spawn tasks to the
    /// current processor's ring.
    ///
    /// Spawned tasks will remain affinitized to the current thread. Spawn
    /// directly on the threadpool object to get a task that will run on any
    /// thread.
    pub fn current_driver(&self) -> &ThreadpoolDriver {
        self.driver(pal::unix::affinity::get_cpu_number())
    }

    /// Returns an object that can be used to submit IOs to the specified ring
    /// in the pool, or to spawn tasks on the specified thread.
    ///
    /// Spawned tasks will remain affinitized to the specified thread. Spawn
    /// directly on the threadpool object to get a task that will run on any
    /// thread.
    pub fn driver(&self, ring_id: u32) -> &ThreadpoolDriver {
        &self.state.drivers[ring_id as usize]
    }

    /// Returns an iterator of drivers for threads that are running and have
    /// their affinity set.
    ///
    /// This is useful for getting a set of drivers that can be used to
    /// parallelize work.
    pub fn active_drivers(&self) -> impl Iterator<Item = &ThreadpoolDriver> + Clone {
        self.state
            .drivers
            .iter()
            .filter(|driver| driver.is_affinity_set())
    }
}

impl Schedule for AffinitizedThreadpoolState {
    fn schedule(&self, runnable: Runnable) {
        self.drivers[pal::unix::affinity::get_cpu_number() as usize]
            .client(Some(runnable.metadata()))
            .schedule(runnable);
    }

    fn name(&self) -> Arc<str> {
        static NAME: OnceLock<Arc<str>> = OnceLock::new();
        NAME.get_or_init(|| "tp".into()).clone()
    }
}

impl Spawn for AffinitizedThreadpool {
    fn scheduler(&self, _metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.state.clone()
    }
}

/// Initiate IOs to the current CPU's thread.
impl Initiate for AffinitizedThreadpool {
    fn initiator(&self) -> &IoInitiator {
        self.current_driver().initiator()
    }
}

/// The state for the thread pool thread for the currently running CPU.
#[derive(Debug, Copy, Clone)]
pub struct Thread {
    _not_send_sync: PhantomData<*const ()>,
}

impl Thread {
    /// Returns an instance for the current CPU.
    pub fn current() -> Option<Self> {
        if !CURRENT_THREAD_DRIVER.with(|current| current.is_lent()) {
            return None;
        }
        Some(Self {
            _not_send_sync: PhantomData,
        })
    }

    /// Calls `f` with the driver for the current thread.
    pub fn with_driver<R>(&self, f: impl FnOnce(&ThreadpoolDriver) -> R) -> R {
        CURRENT_THREAD_DRIVER.with(|current| current.borrow(|driver| f(driver.unwrap())))
    }

    fn with_once<R>(&self, f: impl FnOnce(&ThreadpoolDriver, &ThreadpoolDriverOnce) -> R) -> R {
        self.with_driver(|driver| f(driver, driver.inner.once.get().unwrap()))
    }

    /// Sets the idle task to run. The task is returned by `f`, which receives
    /// the file descriptor of the IO ring.
    ///
    /// The idle task is run before waiting on the IO ring. The idle task can
    /// block synchronously by first calling [`IdleControl::pre_block`], and
    /// then by polling on the IO ring while the task blocks.
    pub fn set_idle_task<F>(&self, f: F)
    where
        F: 'static + Send + AsyncFnOnce(IdleControl),
    {
        self.with_once(|_, once| once.client.set_idle_task(f))
    }

    /// Tries to set the affinity to this thread's intended CPU, if it has not
    /// already been set. Returns `Ok(false)` if the intended CPU is still
    /// offline.
    pub fn try_set_affinity(&self) -> Result<bool, SetAffinityError> {
        self.with_once(|driver, once| {
            let mut state = driver.inner.state.lock();
            if matches!(state.affinity, AffinityState::Set) {
                return Ok(true);
            }
            if !is_cpu_online(driver.inner.cpu).map_err(SetAffinityError::Online)? {
                return Ok(false);
            }

            let mut affinity = CpuSet::new();
            affinity.set(driver.inner.cpu);

            pal::unix::affinity::set_current_thread_affinity(&affinity)
                .map_err(SetAffinityError::Thread)?;
            once.client
                .set_iowq_affinity(&affinity)
                .map_err(SetAffinityError::Ring)?;

            let old_affinity_state = std::mem::replace(&mut state.affinity, AffinityState::Set);
            driver.inner.affinity_set.store(true, Relaxed);
            drop(state);

            match old_affinity_state {
                AffinityState::Waiting(wakers) => {
                    for waker in wakers {
                        waker.wake();
                    }
                }
                AffinityState::Set => unreachable!(),
            }
            Ok(true)
        })
    }

    /// Returns the that caused this thread to spawn.
    ///
    /// Returns `None` if the thread was spawned to issue IO.
    pub fn first_task(&self) -> Option<TaskInfo> {
        self.with_once(|_, once| once.first_task.clone())
    }
}

/// An error that can occur when setting the affinity of a thread.
#[derive(Debug, Error)]
pub enum SetAffinityError {
    /// An error occurred while checking if the CPU is online.
    #[error("failed to check if CPU is online")]
    Online(#[source] io::Error),
    /// An error occurred while setting the thread affinity.
    #[error("failed to set thread affinity")]
    Thread(#[source] io::Error),
    /// An error occurred while setting the IO ring affinity.
    #[error("failed to set io-uring affinity")]
    Ring(#[source] io::Error),
}

thread_local! {
    static CURRENT_THREAD_DRIVER: LoanCell<ThreadpoolDriver> = const { LoanCell::new() };
}

impl SpawnLocal for Thread {
    fn scheduler_local(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.with_driver(|driver| driver.scheduler(metadata).clone())
    }
}

/// A driver for [`AffinitizedThreadpool`] that is targeted at a specific
/// CPU.
#[derive(Debug, Clone, Inspect)]
#[inspect(transparent)]
pub struct ThreadpoolDriver {
    inner: Arc<ThreadpoolDriverInner>,
}

#[derive(Debug, Inspect)]
struct ThreadpoolDriverInner {
    #[inspect(flatten)]
    once: OnceLock<ThreadpoolDriverOnce>,
    #[inspect(skip)]
    builder: Arc<ThreadpoolBuilder>,
    cpu: u32,
    name: Arc<str>,
    affinity_set: AtomicBool,
    #[inspect(flatten)]
    state: Mutex<ThreadpoolDriverState>,
}

#[derive(Debug, Inspect)]
struct ThreadpoolDriverOnce {
    #[inspect(skip)]
    client: PoolClient,
    first_task: Option<TaskInfo>,
}

/// Information about a task that caused a thread to spawn.
#[derive(Debug, Clone, Inspect)]
pub struct TaskInfo {
    /// The name of the task.
    pub name: Arc<str>,
    /// The location of the task.
    #[inspect(display)]
    pub location: &'static std::panic::Location<'static>,
}

#[derive(Debug, Inspect)]
struct ThreadpoolDriverState {
    affinity: AffinityState,
    #[inspect(with = "|x| x.is_some()")]
    notifier: Option<AffinityNotifier>,
    spawned: bool,
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
enum AffinityState {
    #[inspect(transparent)]
    Waiting(#[inspect(with = "|x| x.len()")] Vec<Waker>),
    Set,
}

struct AffinityNotifier(Box<dyn FnOnce(bool) + Send>);

impl std::fmt::Debug for AffinityNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("AffinityNotifier")
    }
}

impl ThreadpoolDriver {
    fn once(&self, metadata: Option<&TaskMetadata>) -> &ThreadpoolDriverOnce {
        self.inner.once.get_or_init(|| {
            let this = self.clone();
            let client = std::thread::spawn(move || {
                let inner = this.inner.clone();
                inner.builder.spawn_pool(inner.cpu, this)
            })
            .join()
            .unwrap()
            .expect("failed to spawn thread pool thread");

            // If no task metadata was provided (because the thread is being
            // spawned to issue IO) use the current task's metadata as the
            // initiating task.
            pal_async::task::with_current_task_metadata(|current_metadata| {
                let metadata = metadata.or(current_metadata);
                ThreadpoolDriverOnce {
                    client,
                    first_task: metadata.map(|metadata| TaskInfo {
                        name: metadata.name().clone(),
                        location: metadata.location(),
                    }),
                }
            })
        })
    }

    fn client(&self, metadata: Option<&TaskMetadata>) -> &PoolClient {
        &self.once(metadata).client
    }

    /// Returns the target CPU number for this thread.
    ///
    /// This may be different from the CPU tasks actually run on if the affinity
    /// has not yet been set for the thread.
    pub fn target_cpu(&self) -> u32 {
        self.inner.cpu
    }

    /// Returns whether this thread's CPU affinity has been set to the intended
    /// CPU.
    pub fn is_affinity_set(&self) -> bool {
        self.inner.affinity_set.load(Relaxed)
    }

    /// Waits for the affinity to be set to this thread's intended CPU. If the
    /// CPU was not online when the thread was created, then this will block
    /// until the CPU is online and someone calls `try_set_affinity`.
    pub async fn wait_for_affinity(&self) {
        // Ensure the thread has been spawned and that the notifier has been
        // called. Use the calling task as the initiating task for diagnostics
        // purposes.
        pal_async::task::with_current_task_metadata(|metadata| self.once(metadata));
        poll_fn(|cx| {
            let mut state = self.inner.state.lock();
            match &mut state.affinity {
                AffinityState::Waiting(wakers) => {
                    if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                        wakers.push(cx.waker().clone());
                    }
                    Poll::Pending
                }
                AffinityState::Set => Poll::Ready(()),
            }
        })
        .await
    }

    /// Sets a function to be called when the thread gets spawned. The function
    /// accepts a single `bool` parameter that indicates that the notifier
    /// should cancel any oustanding run or not. When called by the threadpool,
    /// the function will recieve `true`.
    ///
    /// Return `Err(f)` if the thread is already spawned.
    pub fn set_spawn_notifier<F: 'static + Send + FnOnce(bool)>(&self, f: F) -> Result<(), F> {
        let mut state = self.inner.state.lock();
        if !state.spawned {
            state.notifier = Some(AffinityNotifier(Box::new(f)));
            Ok(())
        } else {
            Err(f)
        }
    }
}

impl Initiate for ThreadpoolDriver {
    fn initiator(&self) -> &IoInitiator {
        self.client(None).initiator()
    }
}

impl Spawn for ThreadpoolDriver {
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.client(Some(metadata)).initiator().scheduler(metadata)
    }
}

impl FdReadyDriver for ThreadpoolDriver {
    type FdReady = FdReady<Self>;

    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady> {
        Ok(FdReady::new(self.clone(), fd))
    }
}

impl WaitDriver for ThreadpoolDriver {
    type Wait = FdWait<Self>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(self.clone(), fd, read_size))
    }
}

impl TimerDriver for ThreadpoolDriver {
    type Timer = Timer<Self>;

    fn new_timer(&self) -> Self::Timer {
        Timer::new(self.clone())
    }
}

/// A driver for [`AffinitizedThreadpool`] that can be retargeted to different
/// CPUs.
#[derive(Debug, Clone)]
pub struct RetargetableDriver {
    inner: Arc<RetargetableDriverInner>,
}

#[derive(Debug)]
struct RetargetableDriverInner {
    threadpool: AffinitizedThreadpool,
    target_cpu: AtomicU32,
}

impl RetargetableDriver {
    /// Returns a new driver, initially targeted to `target_cpu`.
    pub fn new(threadpool: AffinitizedThreadpool, target_cpu: u32) -> Self {
        Self {
            inner: Arc::new(RetargetableDriverInner {
                threadpool,
                target_cpu: target_cpu.into(),
            }),
        }
    }

    /// Retargets the driver to `target_cpu`.
    ///
    /// In-flight IOs will not be retargeted.
    pub fn retarget(&self, target_cpu: u32) {
        self.inner.target_cpu.store(target_cpu, Relaxed);
    }

    /// Returns the current target CPU.
    pub fn current_target_cpu(&self) -> u32 {
        self.inner.target_cpu.load(Relaxed)
    }

    /// Returns the current driver.
    pub fn current_driver(&self) -> &ThreadpoolDriver {
        self.inner.current_driver()
    }
}

impl Initiate for RetargetableDriver {
    fn initiator(&self) -> &IoInitiator {
        self.inner.current_driver().initiator()
    }
}

impl Spawn for RetargetableDriver {
    fn scheduler(&self, _metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.inner.clone()
    }
}

impl RetargetableDriverInner {
    fn current_driver(&self) -> &ThreadpoolDriver {
        self.threadpool.driver(self.target_cpu.load(Relaxed))
    }
}

impl Schedule for RetargetableDriverInner {
    fn schedule(&self, runnable: Runnable) {
        self.current_driver()
            .client(Some(runnable.metadata()))
            .schedule(runnable)
    }

    fn name(&self) -> Arc<str> {
        self.current_driver().inner.name.clone()
    }
}

impl FdReadyDriver for RetargetableDriver {
    type FdReady = FdReady<Self>;

    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady> {
        Ok(FdReady::new(self.clone(), fd))
    }
}

impl WaitDriver for RetargetableDriver {
    type Wait = FdWait<Self>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(self.clone(), fd, read_size))
    }
}

impl TimerDriver for RetargetableDriver {
    type Timer = Timer<Self>;

    fn new_timer(&self) -> Self::Timer {
        Timer::new(self.clone())
    }
}
