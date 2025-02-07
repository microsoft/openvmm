// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

//! Implementation of [`vmbus_client`] traits to communicate with the synic via
//! the Linux HCL driver.

#![warn(missing_docs)]
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
use std::io;
use std::io::IoSliceMut;
use std::os::fd::AsFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::Poll;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_client::SynicClient;
use vmbus_client::VmbusMessageSource;
use zerocopy::IntoBytes;

/// Returns the synic client and message source for use with
/// [`vmbus_client::VmbusClient`].
pub fn new_synic_client_and_messsage_source<T: Driver + ?Sized>(
    driver: &T,
) -> anyhow::Result<(impl SynicClient + use<T>, impl VmbusMessageSource + use<T>)> {
    // Open an HCL vmbus fd for issuing synic requests.
    let hcl_vmbus = Arc::new(HclVmbus::new().context("failed to open hcl_vmbus")?);
    let synic = HclSynic {
        hcl_vmbus: Arc::clone(&hcl_vmbus),
    };

    // Open another one for polling for messages.
    let vmbus_fd = HclVmbus::new()
        .context("failed to open hcl_vmbus")?
        .into_inner();

    let pipe = PolledPipe::new(driver, vmbus_fd).context("failed to created PolledPipe")?;
    let msg_source = MessageSource {
        pipe,
        hcl_vmbus: Arc::clone(&hcl_vmbus),
    };

    Ok((synic, msg_source))
}

struct HclSynic {
    hcl_vmbus: Arc<HclVmbus>,
}

impl SynicClient for HclSynic {
    fn post_message(&self, connection_id: u32, typ: u32, msg: &[u8]) {
        let mut tries = 0;
        let mut wait = 1;
        // If we receive HV_STATUS_INSUFFICIENT_BUFFERS block till the call is
        // successful with a delay.
        loop {
            let ret = self.hcl_vmbus.post_message(connection_id, typ.into(), msg);
            match ret {
                Ok(()) => break,
                Err(HypercallError::Hypervisor(HvError::InsufficientBuffers)) => {
                    tracing::debug!("received HV_STATUS_INSUFFICIENT_BUFFERS, retrying");
                    if tries < 22 {
                        wait *= 2;
                        tries += 1;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(wait / 1000));
                }
                Err(err) => {
                    panic!("received error code from post message call {}", err);
                }
            }
        }
    }

    fn map_event(&self, event_flag: u16, event: &pal_event::Event) -> io::Result<()> {
        self.hcl_vmbus
            .set_eventfd(event_flag.into(), Some(event.as_fd()))
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }

    fn unmap_event(&self, event_flag: u16) {
        self.hcl_vmbus.set_eventfd(event_flag.into(), None).unwrap();
    }

    fn signal_event(&self, connection_id: u32, event_flag: u16) -> io::Result<()> {
        self.hcl_vmbus
            .signal_event(connection_id, event_flag.into())
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }
}

struct MessageSource {
    pipe: PolledPipe,
    hcl_vmbus: Arc<HclVmbus>,
}

impl AsyncRecv for MessageSource {
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

impl VmbusMessageSource for MessageSource {
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
