// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate includes the abstraction layer of Linux SEV-SNP Guest APIs.
#![cfg(target_os = "linux")]
// UNSAFETY: unsafe needed to make ioctl calls.
#![expect(unsafe_code)]

use std::fs::File;
use std::os::fd::AsRawFd;
use thiserror::Error;
use x86defs::snp::SNP_DERIVED_KEY_SIZE;
use x86defs::snp::SNP_GUEST_REQ_MSG_VERSION;
use x86defs::snp::SNP_REPORT_RESP_DATA_SIZE;
use x86defs::snp::SnpDerivedKeyReq;
use x86defs::snp::SnpDerivedKeyResp;
use x86defs::snp::SnpReport;
use x86defs::snp::SnpReportReq;
use x86defs::snp::SnpReportResp;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Ioctl type defined by Linux.
pub const SNP_GUEST_REQ_IOC_TYPE: u8 = b'S';

/// The size of the response data defined by the Linux kernel.
const LINUX_SNP_REPORT_RESP_DATA_SIZE: usize = 4000;

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to open /dev/sev-guest")]
    OpenDevSevGuest(#[source] std::io::Error),
    #[error("SNP_GET_REPORT ioctl failed")]
    SnpGetReportIoctl(#[source] nix::Error),
    #[error("SNP_GET_DERIVED_KEY ioctl failed")]
    SnpGetDerivedKeyIoctl(#[source] nix::Error),
}

/// Ioctl struct defined by Linux.
#[repr(C)]
struct SnpGuestRequestIoctl {
    /// Message version number (must be non-zero).
    msg_version: u32,
    /// Request struct address.
    req_data: u64,
    /// Response struct address.
    resp_data: u64,
    /// VMM error code.
    exitinfo: VmmErrorCode,
}

/// VMM error code.
#[repr(C)]
#[derive(FromZeros, Immutable, KnownLayout)]
struct VmmErrorCode {
    /// Firmware error
    fw_error: u32,
    /// VMM error
    vmm_error: u32,
}

nix::ioctl_readwrite!(
    /// `SNP_GET_REPORT` ioctl defined by Linux.
    snp_get_report,
    SNP_GUEST_REQ_IOC_TYPE,
    0x0,
    SnpGuestRequestIoctl
);

nix::ioctl_readwrite!(
    /// `SNP_GET_DERIVED_KEY` ioctl defined by Linux.
    snp_get_derived_key,
    SNP_GUEST_REQ_IOC_TYPE,
    0x1,
    SnpGuestRequestIoctl
);

/// Response structure for the `SNP_GET_REPORT` ioctl.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct SnpReportIoctlResp {
    /// SNP report as defined by the SEV-SNP ABI spec
    report: SnpReportResp,
    /// Reserved
    _reserved: [u8; LINUX_SNP_REPORT_RESP_DATA_SIZE - SNP_REPORT_RESP_DATA_SIZE],
}

static_assertions::const_assert_eq!(
    LINUX_SNP_REPORT_RESP_DATA_SIZE,
    size_of::<SnpReportIoctlResp>()
);

/// Abstraction of the /dev/sev-guest device.
pub struct SevGuestDevice {
    file: File,
}

impl SevGuestDevice {
    /// Open an /dev/sev-guest device
    pub fn open() -> Result<Self, Error> {
        let sev_guest = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/sev-guest")
            .map_err(Error::OpenDevSevGuest)?;

        Ok(Self { file: sev_guest })
    }

    /// Invoke the `SNP_GET_REPORT` ioctl via the device.
    pub fn get_report(&self, user_data: [u8; 64], vmpl: u32) -> Result<SnpReport, Error> {
        let req = SnpReportReq {
            user_data,
            vmpl,
            rsvd: [0u8; 28],
        };

        let resp = SnpReportIoctlResp::new_zeroed();

        let mut snp_guest_request = SnpGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo: VmmErrorCode::new_zeroed(),
        };

        // SAFETY: Make SNP_GET_REPORT ioctl call to the device with correct types.
        unsafe {
            snp_get_report(self.file.as_raw_fd(), &mut snp_guest_request)
                .map_err(Error::SnpGetReportIoctl)?;
        }

        Ok(resp.report.report)
    }

    /// Invoke the `SNP_GET_DERIVED_KEY` ioctl via the device.
    pub fn get_derived_key(
        &self,
        root_key_select: u32,
        guest_field_select: u64,
        vmpl: u32,
        guest_svn: u32,
        tcb_version: u64,
    ) -> Result<[u8; SNP_DERIVED_KEY_SIZE], Error> {
        let req = SnpDerivedKeyReq {
            root_key_select,
            rsvd: 0u32,
            guest_field_select,
            vmpl,
            guest_svn,
            tcb_version,
        };

        let resp = SnpDerivedKeyResp::new_zeroed();

        let mut snp_guest_request = SnpGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo: VmmErrorCode::new_zeroed(),
        };

        // SAFETY: Make SNP_GET_DERIVED_KEY ioctl call to the device with correct types
        unsafe {
            snp_get_derived_key(self.file.as_raw_fd(), &mut snp_guest_request)
                .map_err(Error::SnpGetReportIoctl)?;
        }

        Ok(resp.derived_key)
    }
}
