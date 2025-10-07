// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]

//! Implementation of [`vmbus_client`] traits to communicate with the synic via
//! the Linux HCL driver.

#![forbid(unsafe_code)]

use anyhow::Context as _;
use futures::AsyncRead;
use hcl::ioctl::HypercallError;
use hcl::vmbus::HclVmbus;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageHeader;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use std::io;
use std::io::IoSliceMut;
use std::os::fd::AsFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_client::PollPostMessage;
use vmbus_client::SynicEventClient;
use vmbus_client::VmbusClientBuilder;
use vmbus_client::VmbusMessageSource;
use zerocopy::IntoBytes;

/// Returns a [`VmbusClientBuilder`] configured to use the Linux HCL driver.
pub fn vmbus_client_builder<T: Driver + ?Sized>(driver: &T) -> anyhow::Result<VmbusClientBuilder> {
    // Open an HCL vmbus fd for issuing synic requests.
    let hcl_vmbus = Arc::new(HclVmbus::new().context("failed to open hcl_vmbus")?);
    let poster = HclSynicPoster {
        hcl_vmbus: Arc::clone(&hcl_vmbus),
        timer: PolledTimer::new(driver),
        deadline: None,
        next_wait: INITIAL_WAIT,
    };
    let synic = HclSynicEvents {
        hcl_vmbus: Arc::clone(&hcl_vmbus),
    };

    // Open another one for polling for messages.
    let vmbus_fd = HclVmbus::new()
        .context("failed to open hcl_vmbus")?
        .into_inner();

    let pipe = PolledPipe::new(driver, vmbus_fd).context("failed to created PolledPipe")?;
    let msg_source = HclMessageSource { pipe, hcl_vmbus };

    Ok(VmbusClientBuilder::new(synic, msg_source, poster))
}

struct HclSynicPoster {
    hcl_vmbus: Arc<HclVmbus>,
    timer: PolledTimer,
    deadline: Option<Instant>,
    next_wait: Duration,
}

const INITIAL_WAIT: Duration = Duration::from_millis(1);

impl PollPostMessage for HclSynicPoster {
    fn poll_post_message(
        &mut self,
        cx: &mut std::task::Context<'_>,
        connection_id: u32,
        typ: u32,
        msg: &[u8],
    ) -> Poll<()> {
        loop {
            if let Some(deadline) = self.deadline {
                ready!(self.timer.poll_until(cx, deadline));
                self.deadline = None;
            }
            // If we receive HV_STATUS_INSUFFICIENT_BUFFERS, the host is backed
            // up in handling these messages. Wait for a while before trying
            // again.
            let ret = self.hcl_vmbus.post_message(connection_id, typ.into(), msg);
            match ret {
                Ok(()) => {
                    self.next_wait = INITIAL_WAIT;
                    break Poll::Ready(());
                }
                Err(HypercallError::Hypervisor(HvError::InsufficientBuffers)) => {
                    tracing::debug!("received HV_STATUS_INSUFFICIENT_BUFFERS, retrying");
                    self.deadline = Some(Instant::now() + self.next_wait);
                    // Wait longer each time.
                    if self.next_wait < Duration::from_secs(1) {
                        self.next_wait *= 2;
                    }
                }
                Err(err) => {
                    panic!("received error code from post message call {}", err);
                }
            }
        }
    }
}

struct HclSynicEvents {
    hcl_vmbus: Arc<HclVmbus>,
}

impl SynicEventClient for HclSynicEvents {
    fn map_event(&self, event_flag: u16, event: &pal_event::Event) -> io::Result<()> {
        self.hcl_vmbus
            .set_eventfd(event_flag.into(), Some(event.as_fd()))
            .map_err(io::Error::other)
    }

    fn unmap_event(&self, event_flag: u16) {
        self.hcl_vmbus.set_eventfd(event_flag.into(), None).unwrap();
    }

    fn signal_event(&self, connection_id: u32, event_flag: u16) -> io::Result<()> {
        self.hcl_vmbus
            .signal_event(connection_id, event_flag.into())
            .map_err(io::Error::other)
    }
}

struct HclMessageSource {
    pipe: PolledPipe,
    hcl_vmbus: Arc<HclVmbus>,
}

impl AsyncRecv for HclMessageSource {
    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
        mut bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut msg = HvMessage::default();
        let size = ready!(Pin::new(&mut self.pipe).poll_read(cx, msg.as_mut_bytes()))?;
        if size == 0 {
            return Ok(0).into();
        }

        assert!(size >= size_of::<HvMessageHeader>());
        let mut remaining = msg.payload();
        let mut total_size = 0;
        while !remaining.is_empty() && !bufs.is_empty() {
            let size = bufs[0].len().min(remaining.len());
            bufs[0][..size].copy_from_slice(&remaining[..size]);
            remaining = &remaining[size..];
            bufs = &mut bufs[1..];
            total_size += size;
        }

        Ok(total_size).into()
    }
}

impl VmbusMessageSource for HclMessageSource {
    fn pause_message_stream(&mut self) {
        self.hcl_vmbus
            .pause_message_stream(true)
            .expect("Unable to disable HCL vmbus message stream.");
    }

    fn resume_message_stream(&mut self) {
        self.hcl_vmbus
            .pause_message_stream(false)
            .expect("Unable to enable HCL vmbus message stream.");
    }
}
