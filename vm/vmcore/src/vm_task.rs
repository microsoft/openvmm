// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for spawning tasks and issuing async IO related to VM
//! activity.

// UNSAFETY: Needed to implement the unsafe new_dyn_overlapped_file method.
#![cfg_attr(windows, expect(unsafe_code))]

use inspect::Inspect;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::TaskMetadata;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;

/// A source for [`VmTaskDriver`]s.
///
/// This is used to create device-specific drivers that implement [`Driver`] and
/// [`Spawn`]. These drivers' behavior can be customized based on the needs of
/// the device.
///
/// The backend for these drivers is customizable for different environments.
#[derive(Clone)]
pub struct VmTaskDriverSource {
    backend: Arc<dyn DynVmBackend>,
}

impl VmTaskDriverSource {
    /// Returns a new task driver source backed by `backend`.
    pub fn new(backend: impl 'static + BuildVmTaskDriver) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Returns a VM task driver with default parameters.
    ///
    /// Use this when you don't care where your task runs.
    pub fn simple(&self) -> VmTaskDriver {
        // Don't provide a name, since backends won't do anything with it for
        // default settings.
        self.builder().build("")
    }

    /// Returns a builder for a custom VM task driver.
    pub fn builder(&self) -> VmTaskDriverBuilder<'_> {
        VmTaskDriverBuilder {
            backend: self.backend.as_ref(),
            run_on_target: false,
            target_vp: None,
        }
    }
}

/// Trait implemented by backends for [`VmTaskDriverSource`].
pub trait BuildVmTaskDriver: Send + Sync {
    /// The associated driver type.
    type Driver: TargetedDriver;

    /// Builds a new driver that can drive IO and spawn tasks.
    fn build(&self, name: String, target_vp: Option<u32>, run_on_target: bool) -> Self::Driver;
}

/// Trait implemented by drivers built with [`BuildVmTaskDriver`].
pub trait TargetedDriver: 'static + Send + Sync + Inspect {
    /// Returns the implementation to use for spawning tasks.
    fn spawner(&self) -> &dyn Spawn;
    /// Returns the implementation to use for driving IO.
    fn driver(&self) -> &dyn Driver;
    /// Retargets the driver to the specified virtual processor.
    fn retarget_vp(&self, target_vp: u32);
    /// Returns whether a driver's target VP is ready for tasks and IO.
    ///
    /// A driver must be operable even if this is false, but the tasks and IO
    /// may run on a different target VP.
    fn is_target_vp_ready(&self) -> bool {
        true
    }
    /// Waits for this driver's target VP to be ready for tasks and IO.
    fn wait_target_vp_ready(&self) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }
}

trait DynTargetedDriver: 'static + Send + Sync + Inspect {
    fn spawner(&self) -> &dyn Spawn;
    fn driver(&self) -> &dyn Driver;
    fn retarget_vp(&self, target_vp: u32);
    fn is_ready(&self) -> bool;
    fn wait_ready(&self) -> Pin<Box<dyn '_ + Future<Output = ()> + Send>>;
}

impl<T: TargetedDriver> DynTargetedDriver for T {
    fn spawner(&self) -> &dyn Spawn {
        self.spawner()
    }

    fn driver(&self) -> &dyn Driver {
        self.driver()
    }

    fn retarget_vp(&self, target_vp: u32) {
        self.retarget_vp(target_vp)
    }

    fn is_ready(&self) -> bool {
        self.is_target_vp_ready()
    }

    fn wait_ready(&self) -> Pin<Box<dyn '_ + Future<Output = ()> + Send>> {
        Box::pin(self.wait_target_vp_ready())
    }
}

trait DynVmBackend: Send + Sync {
    fn build(
        &self,
        name: String,
        target_vp: Option<u32>,
        run_on_target: bool,
    ) -> Arc<dyn DynTargetedDriver>;
}

impl<T: BuildVmTaskDriver> DynVmBackend for T {
    fn build(
        &self,
        name: String,
        target_vp: Option<u32>,
        run_on_target: bool,
    ) -> Arc<dyn DynTargetedDriver> {
        Arc::new(self.build(name, target_vp, run_on_target))
    }
}

/// A builder returned by [`VmTaskDriverSource::builder`].
pub struct VmTaskDriverBuilder<'a> {
    backend: &'a dyn DynVmBackend,
    run_on_target: bool,
    target_vp: Option<u32>,
}

impl VmTaskDriverBuilder<'_> {
    /// A hint to the backend specifies whether the driver should spawned tasks
    /// that always on a thread handling the target VP.
    ///
    /// If `false` (the default), then when spawned tasks are awoken, they may
    /// run on any executor (such as the current one). If `true`, the backend
    /// will run them on the same thread that would drive async IO.
    ///
    /// Some devices will want to override the default to reduce jitter or
    /// ensure that IO is issued from the correct processor.
    pub fn run_on_target(&mut self, run_on_target: bool) -> &mut Self {
        self.run_on_target = run_on_target;
        self
    }

    /// A hint to the backend specifying the guest VP associated with spawned
    /// tasks and IO.
    ///
    /// Backends can use this to ensure that spawned tasks and async IO will run
    /// near or on the target VP.
    pub fn target_vp(&mut self, target_vp: u32) -> &mut Self {
        self.target_vp = Some(target_vp);
        self
    }

    /// Builds a VM task driver.
    ///
    /// `name` is used by some backends to identify a spawned thread. It is
    /// ignored by other backends.
    pub fn build(&self, name: impl Into<String>) -> VmTaskDriver {
        VmTaskDriver {
            inner: self
                .backend
                .build(name.into(), self.target_vp, self.run_on_target),
        }
    }
}

/// A driver returned by [`VmTaskDriverSource`].
///
/// This can be used to spawn tasks (via [`Spawn`]) and issue async IO (via [`Driver`]).
#[derive(Clone, Inspect)]
pub struct VmTaskDriver {
    #[inspect(flatten)]
    inner: Arc<dyn DynTargetedDriver>,
}

impl VmTaskDriver {
    /// Updates the target VP for the task.
    pub fn retarget_vp(&self, target_vp: u32) {
        self.inner.retarget_vp(target_vp)
    }

    /// Returns whether the target VP is ready for tasks and IO.
    ///
    /// A driver must be operable even if this is false, but the tasks and IO
    /// may run on a different target VP.
    pub fn is_target_vp_ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// Waits for the target VP to be ready for tasks and IO.
    pub async fn wait_target_vp_ready(&self) {
        self.inner.wait_ready().await
    }
}

impl Driver for VmTaskDriver {
    fn new_dyn_timer(&self) -> pal_async::driver::PollImpl<dyn pal_async::timer::PollTimer> {
        self.inner.driver().new_dyn_timer()
    }

    #[cfg(unix)]
    fn new_dyn_fd_ready(
        &self,
        fd: std::os::fd::RawFd,
    ) -> std::io::Result<pal_async::driver::PollImpl<dyn pal_async::fd::PollFdReady>> {
        self.inner.driver().new_dyn_fd_ready(fd)
    }

    #[cfg(unix)]
    fn new_dyn_socket_ready(
        &self,
        socket: std::os::fd::RawFd,
    ) -> std::io::Result<pal_async::driver::PollImpl<dyn pal_async::socket::PollSocketReady>> {
        self.inner.driver().new_dyn_socket_ready(socket)
    }

    #[cfg(windows)]
    fn new_dyn_socket_ready(
        &self,
        socket: std::os::windows::io::RawSocket,
    ) -> std::io::Result<pal_async::driver::PollImpl<dyn pal_async::socket::PollSocketReady>> {
        self.inner.driver().new_dyn_socket_ready(socket)
    }

    #[cfg(unix)]
    fn new_dyn_wait(
        &self,
        fd: std::os::fd::RawFd,
        read_size: usize,
    ) -> std::io::Result<pal_async::driver::PollImpl<dyn pal_async::wait::PollWait>> {
        self.inner.driver().new_dyn_wait(fd, read_size)
    }

    #[cfg(windows)]
    fn new_dyn_wait(
        &self,
        handle: std::os::windows::io::RawHandle,
    ) -> std::io::Result<pal_async::driver::PollImpl<dyn pal_async::wait::PollWait>> {
        self.inner.driver().new_dyn_wait(handle)
    }

    #[cfg(windows)]
    unsafe fn new_dyn_overlapped_file(
        &self,
        handle: std::os::windows::io::RawHandle,
    ) -> std::io::Result<
        pal_async::driver::PollImpl<dyn pal_async::windows::overlapped::IoOverlapped>,
    > {
        // SAFETY: passthru from caller
        unsafe { self.inner.driver().new_dyn_overlapped_file(handle) }
    }
}

impl Spawn for VmTaskDriver {
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn pal_async::task::Schedule> {
        self.inner.spawner().scheduler(metadata)
    }
}

/// A backend that spawns all tasks and IO on a single driver.
#[derive(Debug)]
pub struct SingleDriverBackend<T>(T);

impl<T: Driver + Spawn + Clone> SingleDriverBackend<T> {
    /// Returns a new driver backend that spawns all tasks and IO on `driver`,
    /// regardless of policy.
    pub fn new(driver: T) -> Self {
        Self(driver)
    }
}

/// The driver for [`SingleDriverBackend`].
#[derive(Debug)]
pub struct SingleDriver<T>(T);

impl<T> Inspect for SingleDriver<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.ignore();
    }
}

impl<T: Driver + Spawn + Clone> BuildVmTaskDriver for SingleDriverBackend<T> {
    type Driver = SingleDriver<T>;

    fn build(&self, _name: String, _target_vp: Option<u32>, _run_on_target: bool) -> Self::Driver {
        SingleDriver(self.0.clone())
    }
}

impl<T: Driver + Spawn> TargetedDriver for SingleDriver<T> {
    fn spawner(&self) -> &dyn Spawn {
        &self.0
    }

    fn driver(&self) -> &dyn Driver {
        &self.0
    }

    fn retarget_vp(&self, _target_vp: u32) {}
}

pub mod thread {
    //! Provides a thread-based task VM task driver backend
    //! [`ThreadDriverBackend`].

    use super::BuildVmTaskDriver;
    use super::TargetedDriver;
    use inspect::Inspect;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::driver::Driver;
    use pal_async::task::Spawn;

    /// A backend for [`VmTaskDriverSource`](super::VmTaskDriverSource) based on
    /// individual threads.
    ///
    /// If no target VP is specified, this backend will spawn tasks and IO a
    /// default single-threaded IO driver. If a target VP is specified, the
    /// backend will spawn a separate thread and spawn tasks and IOs there.
    #[derive(Debug)]
    pub struct ThreadDriverBackend {
        default_driver: DefaultDriver,
    }

    impl ThreadDriverBackend {
        /// Returns a new backend, using `default_driver` to back task drivers
        /// that did not specify a target VP.
        pub fn new(default_driver: DefaultDriver) -> Self {
            Self { default_driver }
        }
    }

    impl BuildVmTaskDriver for ThreadDriverBackend {
        type Driver = ThreadDriver;

        fn build(
            &self,
            name: String,
            target_vp: Option<u32>,
            _run_on_target: bool,
        ) -> Self::Driver {
            // Build a standalone thread for this device if a target VP was specified.
            if target_vp.is_some() {
                let (_, driver) = DefaultPool::spawn_on_thread(name);
                ThreadDriver {
                    inner: driver,
                    has_dedicated_thread: true,
                }
            } else {
                ThreadDriver {
                    inner: self.default_driver.clone(),
                    has_dedicated_thread: false,
                }
            }
        }
    }

    /// The driver for [`ThreadDriverBackend`].
    #[derive(Debug, Inspect)]
    pub struct ThreadDriver {
        #[inspect(skip)]
        inner: DefaultDriver,
        has_dedicated_thread: bool,
    }

    impl TargetedDriver for ThreadDriver {
        fn spawner(&self) -> &dyn Spawn {
            &self.inner
        }

        fn driver(&self) -> &dyn Driver {
            &self.inner
        }

        fn retarget_vp(&self, _target_vp: u32) {}
    }
}
