// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate includes the abstraction layer of Linux SEV-SNP Guest APIs.
#![cfg(target_os = "linux")]
// UNSAFETY: unsafe needed to make ioctl calls.
#![expect(unsafe_code)]

#[cfg(feature = "dev_snp_ohcl_tio_support")]
use sev_guest_device_tio::TioMsgMmioConfigRsp;
#[cfg(feature = "dev_snp_ohcl_tio_support")]
use sev_guest_device_tio::TioMsgMmioValidateRsp;
#[cfg(feature = "dev_snp_ohcl_tio_support")]
use sev_guest_device_tio::TioMsgSdteWriteRsp;
#[cfg(feature = "dev_snp_ohcl_tio_support")]
use sev_guest_device_tio::TioMsgTdiInfoRsp;
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
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    #[error("TIO_GUEST_REQUEST ioctl failed")]
    TioGuestRequestIoctl(#[source] nix::Error),
}

/// Ioctl struct defined by Linux.
#[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
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

/// TioGuestRequestIoctl struct defined by Linux. In the case of TIO support feature enablement,
/// this structure replaces the SnpGuestRequestIoctl structure.
#[cfg(feature = "dev_snp_ohcl_tio_support")]
#[repr(C)]
pub struct TioGuestRequestIoctl {
    /// Message version number (must be non-zero).
    msg_version: u32,
    /// Request struct address.
    req_data: u64,
    /// Response struct address.
    resp_data: u64,
    /// VMM error code.
    exitinfo1: VmmErrorCode,
    /// TDISP TODO: Exitinfo1
    exitinfo2: u64,
    /// TDISP TODO: tio_msg type
    msg_type: u64,
    /// TDISP TODO: req_size
    req_size: u64,
    /// TDISP TODO: resp_size
    resp_size: u64,
    /// TDISP TODO: pci_id
    pci_id: u64,
    /// TDISP TODO: additional_arg / additional_arg
    additional_arg: u64,
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

#[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
nix::ioctl_readwrite!(
    /// `SNP_GET_REPORT` ioctl defined by Linux.
    snp_get_report,
    SNP_GUEST_REQ_IOC_TYPE,
    0x0,
    SnpGuestRequestIoctl
);

#[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
nix::ioctl_readwrite!(
    /// `SNP_GET_DERIVED_KEY` ioctl defined by Linux.
    snp_get_derived_key,
    SNP_GUEST_REQ_IOC_TYPE,
    0x1,
    SnpGuestRequestIoctl
);

// Feature `dev_snp_ohcl_tio_support` changes the structure definition
// of the sev guest device IOCTL interface to support the TIO_GUEST_REQUEST
// ioctl.
#[cfg(feature = "dev_snp_ohcl_tio_support")]
nix::ioctl_readwrite!(
    /// `SNP_GET_REPORT` ioctl defined by Linux.
    snp_get_report,
    SNP_GUEST_REQ_IOC_TYPE,
    0x0,
    TioGuestRequestIoctl
);

#[cfg(feature = "dev_snp_ohcl_tio_support")]
nix::ioctl_readwrite!(
    /// `SNP_GET_DERIVED_KEY` ioctl defined by Linux.
    snp_get_derived_key,
    SNP_GUEST_REQ_IOC_TYPE,
    0x1,
    TioGuestRequestIoctl
);

#[cfg(feature = "dev_snp_ohcl_tio_support")]
nix::ioctl_readwrite!(
    /// `TIO_GUEST_REQUEST` ioctl defined by Linux.
    tio_guest_request,
    SNP_GUEST_REQ_IOC_TYPE,
    0x3,
    TioGuestRequestIoctl
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

        #[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
        let mut snp_guest_request = SnpGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo: VmmErrorCode::new_zeroed(),
        };

        #[cfg(feature = "dev_snp_ohcl_tio_support")]
        let mut snp_guest_request = TioGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo1: VmmErrorCode::new_zeroed(),
            exitinfo2: 0,
            msg_type: 0,
            req_size: 0,
            resp_size: 0,
            pci_id: 0,
            additional_arg: 0,
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

        #[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
        let mut snp_guest_request = SnpGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo: VmmErrorCode::new_zeroed(),
        };

        #[cfg(feature = "dev_snp_ohcl_tio_support")]
        let mut snp_guest_request = TioGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo1: VmmErrorCode::new_zeroed(),
            exitinfo2: 0,
            msg_type: 0,
            req_size: 0,
            resp_size: 0,
            pci_id: 0,
            additional_arg: 0,
        };

        // SAFETY: Make SNP_GET_DERIVED_KEY ioctl call to the device with correct types
        unsafe {
            snp_get_derived_key(self.file.as_raw_fd(), &mut snp_guest_request)
                .map_err(Error::SnpGetReportIoctl)?;
        }

        Ok(resp.derived_key)
    }

    /// Invoke the `TIO_GUEST_REQUEST` ioctl via the device.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    fn tio_guest_request<RequestType, ResponseType>(
        &mut self,
        msg_type: u64,
        guest_device_id: u16,
        req: RequestType,
    ) -> Result<ResponseType, Error>
    where
        RequestType: IntoBytes + Immutable + std::fmt::Debug,
        ResponseType: FromZeros + IntoBytes + Immutable + std::fmt::Debug,
    {
        let resp = ResponseType::new_zeroed();

        tracing::info!(
            msg = "tio_guest_request issuing ioctl",
            msg_type,
            req = ?req
        );

        let mut snp_guest_request = TioGuestRequestIoctl {
            msg_version: SNP_GUEST_REQ_MSG_VERSION,
            req_data: req.as_bytes().as_ptr() as u64,
            resp_data: resp.as_bytes().as_ptr() as u64,
            exitinfo1: VmmErrorCode::new_zeroed(),
            exitinfo2: 0,
            msg_type,
            req_size: req.as_bytes().len() as u64,
            resp_size: resp.as_bytes().len() as u64,
            pci_id: guest_device_id as u64,
            additional_arg: 0,
        };

        // SAFETY: Make TIO_GUEST_REQUEST ioctl call to the device with correct types
        unsafe {
            tio_guest_request(self.file.as_raw_fd(), &mut snp_guest_request)
                .map_err(Error::TioGuestRequestIoctl)?;
        }

        tracing::info!(
            msg = "tio_guest_request completed successfully",
            resp = ?resp
        );

        Ok(resp)
    }

    /// Invoke the `TIO_MSG_TDI_INFO_REQ` to a given TDISP guest device ID.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub fn tio_msg_tdi_info_req(
        &mut self,
        guest_device_id: u16,
    ) -> Result<TioMsgTdiInfoRsp, Error> {
        use sev_guest_device_tio::TioMsgTdiInfoReq;

        let msg_type = 19; // TIO_MSG_TDI_INFO_REQ

        let req = TioMsgTdiInfoReq {
            guest_device_id,
            _reserved0: [0; 14],
        };

        self.tio_guest_request(msg_type, guest_device_id, req)
    }

    /// Invoke the `TIO_MSG_MMIO_CONFIG_REQ` to a given TDISP guest device ID.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub fn tio_msg_mmio_config_req(
        &mut self,
        guest_device_id: u16,
        range_id: u16,
    ) -> Result<TioMsgMmioConfigRsp, Error> {
        use sev_guest_device_tio::TioMsgMmioConfigReq;

        let msg_type = 23; // TIO_MSG_MMIO_CONFIG_REQ

        let req = TioMsgMmioConfigReq {
            guest_device_id,
            _reserved0: [0; 2],
            flags: 0,
            range_id,
            write: 0,
            _reserved2: [0; 4],
        };

        self.tio_guest_request(msg_type, guest_device_id, req)
    }

    /// Invoke the `TIO_MSG_MMIO_VALIDATE_REQ` to a given TDISP guest device ID.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub fn tio_msg_mmio_validate_req(
        &mut self,
        guest_device_id: u16,
        subrange_base: u64,
        subrange_page_count: u32,
        range_offset: u32,
        range_id: u16,
        validated: bool,
        force_validated: bool,
    ) -> Result<TioMsgMmioValidateRsp, Error> {
        use sev_guest_device_tio::TioMsgMmioValidateReq;
        use sev_guest_device_tio::TioMsgMmioValidateReqFlags;

        let msg_type = 21; // TIO_MSG_MMIO_VALIDATE_REQ 

        let req = TioMsgMmioValidateReq {
            guest_device_id,
            _reserved0: [0; 14],
            subrange_base,
            subrange_page_count,
            range_offset,
            validated_flags: TioMsgMmioValidateReqFlags::new()
                .with_force_validated(force_validated)
                .with_validated(validated),
            range_id,
            _reserved2: [0; 12],
        };

        self.tio_guest_request(msg_type, guest_device_id, req)
    }
    /// Invoke the `TIO_MSG_SDTE_WRITE_REQ` to update the SDTE to allow DMA to the guest.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub fn tio_msg_sdte_write_req(
        &mut self,
        guest_device_id: u16,
    ) -> Result<TioMsgSdteWriteRsp, Error> {
        use sev_guest_device_tio::Sdte;
        use sev_guest_device_tio::SdtePart1;
        use sev_guest_device_tio::SdtePart2;
        use sev_guest_device_tio::SdtePart3;
        use sev_guest_device_tio::TioMsgSdteWriteReq;

        let msg_type = 25; // TIO_MSG_SDTE_WRITE_REQ

        // TDISP TODO: Required value for the current Turin implementation
        let vtom = 0x7fffffff;

        // TDISP TODO: Update with proper VMPL calculation
        let vmpl = 2;
        tracing::info!(
            msg = format!("Sending SDTE write request for VMPL {vmpl:?} and vtom {vtom:x?}...")
        );
        let sdte = Sdte {
            part1: SdtePart1::new().with_v(true).with_ir(true).with_iw(true),
            _reserved0: 0,
            _reserved1: 0,
            part2: SdtePart2::new().with_vmpl(vmpl),
            _reserved2: 0,
            // 2MB PFN
            // TDISP TODO: Update with proper VTOM
            part3: SdtePart3::new().with_vtom_en(true).with_virtual_tom(vtom),
            _reserved3: 0,
            _reserved4: 0,
        };

        // Print sdte as a byte buffer
        tracing::info!(msg = format!("SDTE: {:?}", sdte.as_bytes()));

        let req = TioMsgSdteWriteReq {
            guest_device_id,
            _reserved0: [0; 14],
            sdte,
        };

        self.tio_guest_request(msg_type, guest_device_id, req)
    }
}
