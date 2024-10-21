// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Single-threaded task pools backed by platform-specific IO backends.

use crate::task::task_queue;
use crate::task::Schedule;
use crate::task::Scheduler;
use crate::task::Spawn;
use crate::task::TaskMetadata;
use crate::task::TaskQueue;
use std::future::poll_fn;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

/// An single-threaded task pool backed by IO backend `T`.
#[derive(Debug)]
pub struct IoPool<T> {
    driver: IoDriver<T>,
    tasks: TaskQueue,
}

/// A driver to spawn tasks and IO objects on [`IoPool`].
#[derive(Debug)]
pub struct IoDriver<T> {
    pub(crate) inner: Arc<T>,
    scheduler: Arc<Scheduler>,
}

impl<T> Clone for IoDriver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            scheduler: self.scheduler.clone(),
        }
    }
}

/// Trait implemented by IO backends.
pub trait IoBackend: Send + Sync {
    /// The name of the backend.
    fn name() -> &'static str;
    /// Run the
    fn run<Fut: Future>(self: &Arc<Self>, fut: Fut) -> Fut::Output;
}

impl<T: IoBackend + Default> IoPool<T> {
    /// Creates a new task pool.
    pub fn new() -> Self {
        Self::named(T::name().to_owned())
    }

    fn named(name: impl Into<Arc<str>>) -> Self {
        let (tasks, scheduler) = task_queue(name);
        Self {
            driver: IoDriver {
                inner: Arc::new(T::default()),
                scheduler: Arc::new(scheduler),
            },
            tasks,
        }
    }

    /// Creates and runs a task pool, seeding it with an initial future
    /// `f(driver)`, until all tasks have completed.
    pub fn run_with<F, Fut>(f: F) -> Fut::Output
    where
        F: FnOnce(IoDriver<T>) -> Fut,
        Fut: Future + Send,
        Fut::Output: 'static + Send,
    {
        let mut pool = Self::named(std::thread::current().name().unwrap_or_else(|| T::name()));
        let fut = f(pool.driver());
        drop(pool.driver.scheduler);
        pool.driver
            .inner
            .run(async { futures::future::join(fut, pool.tasks.run()).await.0 })
    }
}

impl<T: IoBackend> IoPool<T> {
    /// Returns the IO driver.
    pub fn driver(&self) -> IoDriver<T> {
        // TODO: return by reference?
        self.driver.clone()
    }

    /// Runs `f` and the task pool until `f` completes.
    pub fn run_until<Fut: Future>(&mut self, f: Fut) -> Fut::Output {
        let mut tasks = pin!(self.tasks.run());
        let mut f = pin!(f);
        self.driver.inner.run(poll_fn(|cx| {
            if let Poll::Ready(r) = f.as_mut().poll(cx) {
                Poll::Ready(r)
            } else {
                assert!(tasks.as_mut().poll(cx).is_pending());
                Poll::Pending
            }
        }))
    }

    /// Runs the task pool until all tasks are completed.
    pub fn run(mut self) {
        // Update the executor name with the current thread's name.
        if let Some(name) = std::thread::current().name() {
            self.driver.scheduler.set_name(name);
        }
        drop(self.driver.scheduler);
        self.driver.inner.run(self.tasks.run())
    }
}

impl<T: IoBackend> Spawn for IoDriver<T> {
    fn scheduler(&self, _metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.scheduler.clone()
    }
}
