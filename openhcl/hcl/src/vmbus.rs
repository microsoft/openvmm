// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for the `/dev/mshv_sint` device.

use crate::ioctl::HypercallError;
use crate::ioctl::IoctlError;
use std::fs::File;
use std::os::unix::prelude::*;

mod ioctl {
    #![allow(non_camel_case_types)]

    use nix::ioctl_write_ptr;
    use std::os::unix::prelude::*;

    const MSHV_IOCTL: u8 = 0xb8;
    const MSHV_SINT_SIGNAL_EVENT: u16 = 0x22;
    const MSHV_SINT_POST_MESSAGE: u16 = 0x23;
    const MSHV_SINT_SET_EVENTFD: u16 = 0x24;
    const MSHV_SINT_PAUSE_MESSAGE_STREAM: u16 = 0x25;

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    pub struct hcl_post_message {
        pub message_type: u64,
        pub connection_id: u32,
        pub payload_size: u32,
        pub payload: *const u8,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    pub struct hcl_signal_event {
        pub connection_id: u32,
        pub flag: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    pub struct hcl_set_eventfd {
        pub fd: RawFd,
        pub flag: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, Default)]
    pub struct hcl_pause_message_stream {
        pub pause: u8,
        pub _reserved: [u8; 7],
    }

    ioctl_write_ptr!(
        hcl_post_message,
        MSHV_IOCTL,
        MSHV_SINT_POST_MESSAGE,
        hcl_post_message
    );

    ioctl_write_ptr!(
        hcl_signal_event,
        MSHV_IOCTL,
        MSHV_SINT_SIGNAL_EVENT,
        hcl_signal_event
    );

    ioctl_write_ptr!(
        hcl_set_eventfd,
        MSHV_IOCTL,
        MSHV_SINT_SET_EVENTFD,
        hcl_set_eventfd
    );

    ioctl_write_ptr!(
        hcl_pause_message_stream,
        MSHV_IOCTL,
        MSHV_SINT_PAUSE_MESSAGE_STREAM,
        hcl_pause_message_stream
    );
}

/// Device used to interact with a synic sint.
pub struct HclVmbus {
    file: File,
}

impl HclVmbus {
    /// Opens a new instance.
    pub fn new() -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mshv_sint")?;

        Ok(Self { file })
    }

    /// Returns the backing file.
    pub fn into_inner(self) -> File {
        self.file
    }

    /// Attempts to post a message to a given connection ID using the HvPostMessage hypercall.
    pub fn post_message(
        &self,
        connection_id: u32,
        message_type: u64,
        message: &[u8],
    ) -> Result<(), HypercallError> {
        tracing::trace!(connection_id, "posting message");

        let post_message = ioctl::hcl_post_message {
            message_type,
            connection_id,
            payload_size: message.len() as u32,
            payload: message.as_ptr(),
        };

        // SAFETY: calling IOCTL as documented, with no special requirements.
        let result = unsafe { ioctl::hcl_post_message(self.file.as_raw_fd(), &post_message) };
        HypercallError::check(result)
    }

    /// Attempts to signal a given event connection ID using the HvSignalEvent hypercall.
    pub fn signal_event(&self, connection_id: u32, flag: u32) -> Result<(), HypercallError> {
        tracing::trace!(connection_id, flag, "signaling event");

        let signal_event = ioctl::hcl_signal_event {
            connection_id,
            flag,
        };

        // SAFETY: calling IOCTL as documented, with no special requirements.
        let result = unsafe { ioctl::hcl_signal_event(self.file.as_raw_fd(), &signal_event) };
        HypercallError::check(result)
    }

    /// Sets an eventfd to be signaled when event `flag` is signaled by the
    /// hypervisor on SINT 7.
    pub fn set_eventfd(&self, flag: u32, event: Option<BorrowedFd<'_>>) -> Result<(), IoctlError> {
        tracing::trace!(flag, ?event, "setting event fd");

        let set_eventfd = ioctl::hcl_set_eventfd {
            flag,
            fd: event.map_or(-1, |e| e.as_raw_fd()),
        };

        // SAFETY: Event is either None or a valid and open fd.
        unsafe { ioctl::hcl_set_eventfd(self.file.as_raw_fd(), &set_eventfd).map_err(IoctlError) }?;
        Ok(())
    }

    /// Indicate whether new messages should be accepted from the host.
    ///
    /// The primary purpose of this is to prevent new messages from arriving when saving.
    ///
    /// When paused, the SINT will be masked, preventing the host from sending new messages. Reading
    /// from the device will return messages already in the slot, and then return EOF once all
    /// messages are cleared.
    ///
    /// When resumed, the SINT is unmasked and reading from the message slot will block until new
    /// messages arrive.
    pub fn pause_message_stream(&self, pause: bool) -> Result<(), IoctlError> {
        tracing::trace!(?pause, "pausing message stream");

        let pause_message_stream = ioctl::hcl_pause_message_stream {
            pause: pause.into(),
            _reserved: [0; 7],
        };

        // SAFETY: ioctl has no prerequisites.
        unsafe {
            ioctl::hcl_pause_message_stream(self.file.as_raw_fd(), &pause_message_stream)
                .map_err(IoctlError)?;
        }

        Ok(())
    }
}
