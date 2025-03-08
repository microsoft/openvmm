// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a listener to wait for Linux kobject uevents.
//!
//! These are used to wait for device hotplug events, disk capacity changes, and
//! other asynchronous hardware state changes in Linux.

#![cfg(target_os = "linux")]

mod bind_kobject_uevent;

use anyhow::Context;
use fs_err::PathExt;
use futures::AsyncReadExt;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Task;
use socket2::Socket;
use std::future::Future;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

/// A listener for Linux udev events.
pub struct UeventListener {
    _task: Task<()>,
    send: mesh::Sender<TaskRequest>,
}

/// An error from [`UeventListener::new`].
#[derive(Debug, Error)]
#[error("failed to create uevent socket")]
pub struct NewUeventListenerError(#[source] io::Error);

impl UeventListener {
    /// Opens a new netlink socket and starts listening on it.
    pub fn new(driver: &impl SpawnDriver) -> Result<Self, NewUeventListenerError> {
        let socket =
            bind_kobject_uevent::bind_kobject_uevent_socket().map_err(NewUeventListenerError)?;
        let socket = PolledSocket::new(driver, socket).map_err(NewUeventListenerError)?;
        let (send, recv) = mesh::mpsc_channel();
        let thing = ListenerTask {
            socket,
            callbacks: Vec::new(),
            recv,
            next_id: 0,
        };
        let task = driver.spawn("uevent", async move { thing.run().await });
        Ok(Self { _task: task, send })
    }

    /// Adds a callback function that receives every event.
    pub async fn add_custom_callback(
        &self,
        callback: impl 'static + Send + FnMut(Notification<'_>),
    ) -> CallbackHandle {
        self.send
            .call(TaskRequest::NewFilter, Box::new(callback))
            .await
            .unwrap()
    }

    /// Adds a callback that runs when the block device with the given
    /// major/minor numbers has been resized or a rescan event was triggered
    /// where the caller is required to rescan for the condition
    pub async fn add_block_resize_callback(
        &self,
        major: u32,
        minor: u32,
        mut notify: impl 'static + Send + FnMut(),
    ) -> CallbackHandle {
        self.add_custom_callback(move |event| match event {
            Notification::Event(kvs) => {
                if (kvs.get("RESCAN") == Some("true"))
                    || (kvs.get("RESIZE") == Some("1")
                        && kvs.get("SUBSYSTEM") == Some("block")
                        && kvs.get("ACTION") == Some("change")
                        && kvs.get("MAJOR").is_some_and(|x| x.parse() == Ok(major))
                        && kvs.get("MINOR").is_some_and(|x| x.parse() == Ok(minor)))
                {
                    notify();
                }
            }
        })
        .await
    }

    /// Waits for a child of the provided devpath (typically something under
    /// /sys) to exist.
    ///
    /// If it does not immediately exist, this will poll the path for existence
    /// each time a new uevent arrives.
    ///
    /// `f` will be called with the file name of the child, and a boolean: true
    /// if the child was found by uevent, false if it was found by sysfs. It
    /// should return `Some(_)` if the child is the correct one.
    ///
    /// This is inefficient if there are lots of waiters and lots of incoming
    /// uevents, but this is not an expected use case.
    pub async fn wait_for_matching_child<T: 'static + Send, F, Fut>(
        &self,
        path: &Path,
        f: F,
    ) -> io::Result<T>
    where
        F: Fn(PathBuf, bool) -> Fut,
        Fut: Future<Output = Option<T>>,
    {
        let scan_for_matching_child = || async {
            for entry in path.fs_err_read_dir()? {
                let entry = entry?;
                if let Some(r) = f(entry.path(), false).await {
                    return Ok::<Option<T>, io::Error>(Some(r));
                }
            }
            Ok(None)
        };

        // Fast path.
        if path.exists() {
            if let Some(child) = scan_for_matching_child().await? {
                return Ok(child);
            }
        }

        // Get the absolute devpath to make child lookups fast.
        self.wait_for_devpath(path).await?;
        let path = path.fs_err_canonicalize()?;
        let path_clone = path.clone();
        let parent_devpath = path
            .strip_prefix("/sys")
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid devpath"))?
            .to_path_buf();

        let (send, mut recv) = mesh::channel();
        let _handle = self
            .add_custom_callback({
                move |notification| {
                    match notification {
                        Notification::Event(uevent) => {
                            // uevent can return a rescan event in some cases where it is not sure
                            // about the end state. In those cases, the end state needs to be checked
                            // again for any change.
                            if uevent.get("RESCAN") == Some("true") {
                                if let Ok(read_dir) = path_clone.fs_err_read_dir() {
                                    for entry in read_dir {
                                        if let Ok(sub_entry) = entry {
                                            send.send((sub_entry.path(), false));
                                        }
                                    }
                                }
                            } else if uevent.get("ACTION") == Some("add") {
                                let Some(devpath) = uevent.get("DEVPATH") else {
                                    return;
                                };
                                // Remove the leading /.
                                let devpath = Path::new(&devpath[1..]);
                                if devpath.parent() == Some(&parent_devpath) {
                                    send.send((Path::new("/sys").join(devpath), true));
                                }
                            }
                        }
                    }
                }
            })
            .await;

        if let Some(child) = scan_for_matching_child().await? {
            return Ok(child);
        }

        tracing::debug!(path = %path.display(), "waiting for child nodes");
        while let Some((path, is_uevent)) = recv.next().await {
            if let Some(r) = f(path, is_uevent).await {
                return Ok(r);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Did not find a matching path",
        ))
    }

    /// Waits for the provided devpath (typically something under /sys) to
    /// exist.
    ///
    /// If it does not immediately exist, this will poll the path for existence
    /// each time a new uevent arrives.
    ///
    /// This is inefficient if there are lots of waiters and lots of incoming
    /// uevents, but this is not an expected use case.
    pub async fn wait_for_devpath(&self, path: &Path) -> io::Result<()> {
        // Fast path.
        if path.exists() {
            return Ok(());
        }

        // Register the listener.
        let (send, recv) = mesh::oneshot();
        let _handle = self
            .add_custom_callback({
                let path = path.to_owned();
                let mut send = Some(send);
                move |event| {
                    if send.is_none() {
                        return;
                    }
                    match event {
                        Notification::Event(uevent) => {
                            if (uevent.get("ACTION") == Some("add"))
                                || (uevent.get("RESCAN") == Some("true"))
                            {
                                let r = path.fs_err_symlink_metadata();
                                if !matches!(&r, Err(err) if err.kind() == io::ErrorKind::NotFound)
                                {
                                    send.take().unwrap().send(r);
                                }
                            }
                        }
                    }
                }
            })
            .await;

        // Check for the path again in case it arrived before the listener was
        // registered.
        let r = match path.fs_err_symlink_metadata() {
            Ok(m) => Ok(m),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "waiting for devpath");
                recv.await.unwrap()
            }
            Err(err) => Err(err),
        };
        r?;
        Ok(())
    }
}

/// A notification for a [`UeventListener`] callback to process.
pub enum Notification<'a> {
    /// An event arrived.
    Event(&'a Uevent<'a>),
}

/// A device event.
pub struct Uevent<'a> {
    header: &'a str,
    properties: Vec<(&'a str, &'a str)>,
}

impl Uevent<'_> {
    /// Gets the header.
    pub fn header(&self) -> &str {
        self.header
    }

    /// Gets a property by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        let i = self
            .properties
            .binary_search_by_key(&key, |(k, _)| k)
            .ok()?;
        Some(self.properties[i].1)
    }
}

/// A callback handle from [`UeventListener`].
///
/// When dropped, it will unregister the callback. This is asynchronous, so the
/// callback may be called several more times after this.
#[must_use]
#[derive(Debug)]
pub struct CallbackHandle {
    id: u64,
    send: mesh::Sender<TaskRequest>,
}

impl Drop for CallbackHandle {
    fn drop(&mut self) {
        self.send.send(TaskRequest::RemoveFilter(self.id))
    }
}

enum TaskRequest {
    NewFilter(Rpc<Box<dyn Send + FnMut(Notification<'_>)>, CallbackHandle>),
    RemoveFilter(u64),
}

struct ListenerTask {
    socket: PolledSocket<Socket>,
    callbacks: Vec<Filter>,
    recv: mesh::Receiver<TaskRequest>,
    next_id: u64,
}

struct Filter {
    id: u64,
    func: Box<dyn Send + FnMut(Notification<'_>)>,
}

impl ListenerTask {
    async fn run(self) {
        if let Err(err) = self.run_inner().await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "uevent failure"
            );
        }
    }

    async fn run_inner(mut self) -> anyhow::Result<()> {
        let mut buf = [0; 4096];

        enum Event {
            Request(Option<TaskRequest>),
            Read(io::Result<usize>),
        }

        loop {
            let event = (
                self.socket.read(&mut buf).map(Event::Read),
                self.recv.next().map(Event::Request),
            )
                .race()
                .await;

            match event {
                Event::Request(Some(request)) => match request {
                    TaskRequest::NewFilter(rpc) => rpc.handle_sync(|filter_fn| {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.callbacks.push(Filter {
                            func: filter_fn,
                            id,
                        });
                        CallbackHandle {
                            id,
                            send: self.recv.sender(),
                        }
                    }),
                    TaskRequest::RemoveFilter(id) => {
                        self.callbacks
                            .swap_remove(self.callbacks.iter().position(|f| f.id == id).unwrap());
                    }
                },
                Event::Request(None) => break Ok(()),
                Event::Read(r) => {
                    match r {
                        Ok(n) => {
                            let buf = std::str::from_utf8(&buf[..n])
                                .context("failed to parse uevent as utf-8 string")?;
                            let uevent = parse_uevent(buf)?;
                            for callback in &mut self.callbacks {
                                (callback.func)(Notification::Event(&uevent));
                            }
                        }
                        Err(e) => {
                            // uevent socket is an unreliable source and in some cases (such as an
                            // uevent flood) can overflow. Two ways to handle that. Either increase
                            // the socket buffer size and hope that buffer doesn't overflow or wake up
                            // the callers to have them rescan for the condition. We went with the latter
                            // here as that has a higher degree of reliability.
                            if let Some(libc::ENOBUFS) = e.raw_os_error() {
                                tracing::info!("uevent socket read error: {:?}", e);
                                let properties: Vec<(&str, &str)> = vec![("RESCAN", "true")];
                                let uevent = Uevent {
                                    header: "rescan",
                                    properties,
                                };
                                for callback in &mut self.callbacks {
                                    (callback.func)(Notification::Event(&uevent));
                                }
                            } else {
                                Err(e).context("uevent read failure")?;
                            }
                        }
                    };
                }
            }
        }
    }
}

fn parse_uevent(buf: &str) -> anyhow::Result<Uevent<'_>> {
    let mut lines = buf.split('\0');
    let header = lines.next().context("missing event header")?;
    let properties = lines.filter_map(|line| line.split_once('=')).collect();
    tracing::debug!(header, ?properties, "uevent");
    let mut uevent = Uevent { header, properties };
    uevent.properties.sort_by_key(|(k, _)| *k);
    Ok(uevent)
}
