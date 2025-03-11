// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module implements the Linux TDX Guest APIs based on ioctl.

// UNSAFETY: unsafe needed to make ioctl calls.
#![expect(unsafe_code)]

use crate::protocol;
use std::fs::File;
use std::os::fd::AsRawFd;
use thiserror::Error;
use zerocopy::FromZeros;

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to open /dev/tdx_guest")]
    OpenDevTdxGuest(#[source] std::io::Error),
    #[error("TDX_CMD_GET_REPORT0 ioctl failed")]
    TdxGetReportIoctl(#[source] nix::Error),
}

nix::ioctl_readwrite!(
    /// `TDX_CMD_GET_REPORT0` ioctl defined by Linux.
    tdx_get_report0,
    protocol::TDX_CMD_GET_REPORT0_IOC_TYPE,
    0x1,
    protocol::TdxReportReq
);

/// Abstraction of the /dev/tdx_guest device.
pub struct TdxGuestDevice {
    file: File,
}

impl TdxGuestDevice {
    /// Open an /dev/tdx_guest device
    pub fn open() -> Result<Self, Error> {
        let tdx_guest = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tdx_guest")
            .map_err(Error::OpenDevTdxGuest)?;

        Ok(Self { file: tdx_guest })
    }

    /// Invoke the `TDX_CMD_GET_REPORT0` ioctl via the device.
    pub fn get_report(
        &self,
        report_data: [u8; 64],
        _vmpl: u32,
    ) -> Result<protocol::TdReport, Error> {
        let mut tdx_report_request = protocol::TdxReportReq {
            report_data,
            td_report: protocol::TdReport::new_zeroed(),
        };

        // SAFETY: Make TDX_CMD_GET_REPORT0 ioctl call to the device with correct types.
        unsafe {
            tdx_get_report0(self.file.as_raw_fd(), &mut tdx_report_request)
                .map_err(Error::TdxGetReportIoctl)?;
        }

        Ok(tdx_report_request.td_report)
    }
}
