// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::chk_win32;
use disk_backend::UnmapBehavior;
use inspect::Inspect;
use scsi_defs as scsi;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
use windows_sys::Win32::Storage::Vhd;
use zerocopy::{FromBytes, FromZeros, IntoBytes};

#[derive(Debug, thiserror::Error)]
#[error("SCSI command {opcode:#x} failed: status {status:#x}, sense {sense:02x?}")]
struct ScsiCommandError {
    opcode: u8,
    status: u8,
    sense: Vec<u8>,
}

fn command(
    handle: BorrowedHandle<'_>,
    cdb: &mut [u8],
    data: &mut [u8],
    data_in: bool,
    srb_flags: u32,
) -> io::Result<usize> {
    let mut sense = [0; size_of::<scsi::SenseData>()];
    let parameters = Vhd::RAW_SCSI_VIRTUAL_DISK_PARAMETERS {
        Version: Vhd::RAW_SCSI_VIRTUAL_DISK_VERSION_1,
        Anonymous: Vhd::RAW_SCSI_VIRTUAL_DISK_PARAMETERS_0 {
            Version1: Vhd::RAW_SCSI_VIRTUAL_DISK_PARAMETERS_0_0 {
                RSVDHandle: 0,
                DataIn: data_in.into(),
                CdbLength: cdb.len().try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "SCSI CDB too long")
                })?,
                SenseInfoLength: sense.len() as u8,
                SrbFlags: srb_flags,
                DataTransferLength: data.len().try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "SCSI data buffer too long")
                })?,
                DataBuffer: data.as_mut_ptr().cast(),
                SenseInfo: sense.as_mut_ptr(),
                Cdb: cdb.as_mut_ptr(),
            },
        },
    };
    let mut response = Vhd::RAW_SCSI_VIRTUAL_DISK_RESPONSE {
        Version: Vhd::RAW_SCSI_VIRTUAL_DISK_VERSION_1,
        ..Default::default()
    };
    // SAFETY: The handle remains borrowed and all buffers stay alive for this
    // synchronous call. Version1 is initialized, and all lengths match the buffers.
    unsafe {
        chk_win32(Vhd::RawSCSIVirtualDisk(
            handle.as_raw_handle(),
            &parameters,
            Vhd::RAW_SCSI_VIRTUAL_DISK_FLAG_NONE,
            &mut response,
        ))?;
    }
    // SAFETY: Both the request and response use version 1.
    let response = unsafe { response.Anonymous.Version1 };
    if response.DataTransferLength as usize > data.len()
        || response.SenseInfoLength as usize > sense.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SCSI response lengths",
        ));
    }
    if response.ScsiStatus != scsi::ScsiStatus::GOOD.0 {
        return Err(io::Error::other(ScsiCommandError {
            opcode: cdb[0],
            status: response.ScsiStatus,
            sense: sense[..response.SenseInfoLength as usize].to_vec(),
        }));
    }
    Ok(response.DataTransferLength as usize)
}

fn query_vpd<'a>(
    handle: BorrowedHandle<'_>,
    page: u8,
    buffer: &'a mut [u8; 255],
) -> io::Result<&'a [u8]> {
    let mut cdb = scsi::CdbInquiry {
        operation_code: scsi::ScsiOp::INQUIRY.0,
        flags: scsi::InquiryFlags::new().with_vpd(true),
        page_code: page,
        allocation_length: (buffer.len() as u16).into(),
        control: 0,
    };
    let length = command(handle, cdb.as_mut_bytes(), buffer, true, 0)?;
    if length < 4 || buffer[1] != page {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SCSI VPD page header",
        ));
    }
    let page_length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
    buffer[..length]
        .get(4..4 + page_length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated SCSI VPD page"))
}

#[derive(Debug, Inspect)]
pub(crate) struct UnmapCapabilities {
    pub behavior: UnmapBehavior,
    pub max_sectors: u32,
    pub optimal_sectors: u32,
}

impl UnmapCapabilities {
    const UNSUPPORTED: Self = Self {
        behavior: UnmapBehavior::Ignored,
        max_sectors: 0,
        optimal_sectors: 0,
    };

    fn from_vpd(
        provisioning: &scsi::VpdLogicalBlockProvisioningPage,
        limits: &scsi::VpdBlockLimitsDescriptor,
    ) -> Self {
        if provisioning.flags & 0x80 == 0
            || limits.max_unmap_lba_count.get() == 0
            || limits.max_unmap_block_descriptor_count.get() == 0
        {
            return Self::UNSUPPORTED;
        }
        Self {
            behavior: if provisioning.flags & 0x04 != 0 {
                UnmapBehavior::Zeroes
            } else {
                UnmapBehavior::Unspecified
            },
            max_sectors: limits.max_unmap_lba_count.get(),
            optimal_sectors: limits.optimal_unmap_granularity.get().max(1),
        }
    }
}

pub(crate) fn unmap_capabilities(handle: BorrowedHandle<'_>) -> io::Result<UnmapCapabilities> {
    let mut buffer = [0; 255];
    let pages = match query_vpd(handle, scsi::VPD_SUPPORTED_PAGES, &mut buffer) {
        // Older drivers may not implement SCSI passthrough.
        Err(err) if err.raw_os_error() == Some(ERROR_NOT_SUPPORTED as i32) => {
            return Ok(UnmapCapabilities::UNSUPPORTED);
        }
        result => result?,
    };
    if !pages.contains(&scsi::VPD_LOGICAL_BLOCK_PROVISIONING)
        || !pages.contains(&scsi::VPD_BLOCK_LIMITS)
    {
        return Ok(UnmapCapabilities::UNSUPPORTED);
    }
    let (provisioning, _) = scsi::VpdLogicalBlockProvisioningPage::read_from_prefix(query_vpd(
        handle,
        scsi::VPD_LOGICAL_BLOCK_PROVISIONING,
        &mut buffer,
    )?)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated provisioning page"))?;
    if provisioning.flags & 0x80 == 0 {
        return Ok(UnmapCapabilities::UNSUPPORTED);
    }
    let (limits, _) = scsi::VpdBlockLimitsDescriptor::read_from_prefix(query_vpd(
        handle,
        scsi::VPD_BLOCK_LIMITS,
        &mut buffer,
    )?)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "truncated block limits page"))?;
    Ok(UnmapCapabilities::from_vpd(&provisioning, &limits))
}

pub(crate) fn unmap(
    handle: BorrowedHandle<'_>,
    sector: u64,
    count: u32,
    block_level_only: bool,
) -> io::Result<()> {
    let header = scsi::UnmapListHeader {
        data_length: 22.into(),
        block_descriptor_data_length: 16.into(),
        ..FromZeros::new_zeroed()
    };
    let range = scsi::UnmapBlockDescriptor {
        start_lba: sector.into(),
        lba_count: count.into(),
        ..FromZeros::new_zeroed()
    };
    let mut data = [0; 24];
    data[..8].copy_from_slice(header.as_bytes());
    data[8..].copy_from_slice(range.as_bytes());
    let mut cdb = scsi::Unmap {
        operation_code: scsi::ScsiOp::UNMAP,
        allocation_length: (data.len() as u16).into(),
        ..FromZeros::new_zeroed()
    };
    command(
        handle,
        cdb.as_mut_bytes(),
        &mut data,
        false,
        if block_level_only {
            scsi::SRB_FLAGS_BLOCK_LEVEL_ONLY
        } else {
            0
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsHandle;

    #[test]
    fn unmap_requires_advertised_support_and_limits() {
        let provisioning = scsi::VpdLogicalBlockProvisioningPage {
            flags: 0x80,
            ..FromZeros::new_zeroed()
        };
        let limits = scsi::VpdBlockLimitsDescriptor {
            max_unmap_lba_count: 4096.into(),
            max_unmap_block_descriptor_count: 1.into(),
            ..FromZeros::new_zeroed()
        };
        let supported = UnmapCapabilities::from_vpd(&provisioning, &limits);
        assert_eq!(supported.behavior, UnmapBehavior::Unspecified);
        assert_eq!(supported.max_sectors, 4096);
        assert_eq!(supported.optimal_sectors, 1);
        for (flags, sectors, descriptors) in [(0x04, 4096, 1), (0x80, 0, 1), (0x80, 4096, 0)] {
            let provisioning = scsi::VpdLogicalBlockProvisioningPage {
                flags,
                ..FromZeros::new_zeroed()
            };
            let limits = scsi::VpdBlockLimitsDescriptor {
                max_unmap_lba_count: sectors.into(),
                max_unmap_block_descriptor_count: descriptors.into(),
                ..FromZeros::new_zeroed()
            };
            let unsupported = UnmapCapabilities::from_vpd(&provisioning, &limits);
            assert_eq!(unsupported.behavior, UnmapBehavior::Ignored);
            assert_eq!(unsupported.max_sectors, 0);
        }
        let provisioning = scsi::VpdLogicalBlockProvisioningPage {
            flags: 0x84,
            ..FromZeros::new_zeroed()
        };
        assert_eq!(
            UnmapCapabilities::from_vpd(&provisioning, &limits).behavior,
            UnmapBehavior::Zeroes
        );
    }

    #[test]
    fn scsi_failure_is_not_transport_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scsi.vhdx");
        drop(crate::Vhd::create_dynamic(&path, 64, true).unwrap());
        let vhd = crate::VhdmpDisk::options().open(&path).unwrap();
        let error = command(
            vhd.0.as_handle(),
            &mut [0x12, 1, 0xff, 0, 64, 0],
            &mut [0; 64],
            true,
            0,
        )
        .unwrap_err();
        let status = error
            .get_ref()
            .unwrap()
            .downcast_ref::<ScsiCommandError>()
            .unwrap();
        assert_eq!(status.status, scsi::ScsiStatus::CHECK_CONDITION.0);
        assert!(!status.sense.is_empty());
    }

    #[test]
    fn unmap_transport_failure_is_reported() {
        let file = tempfile::tempfile().unwrap();
        assert!(unmap(file.as_handle(), 0, 1, false).is_err());
    }
}
