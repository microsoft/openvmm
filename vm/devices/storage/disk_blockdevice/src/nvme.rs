// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module forms NVMe cmds and sends to Linux kernel.

use super::BlockDevice;
use super::DeviceType;
use bitfield_struct::bitfield;
use blocking::unblock;
use disk_backend::pr::ReservationReport;
use nvme_common::from_nvme_reservation_report;
use nvme_spec::nvm;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

mod ioctl {
    use nix::ioctl_readwrite;

    #[repr(C)]
    #[derive(Debug)]
    pub struct NvmeCmd {
        pub opcode: u8,
        pub flags: u8,
        pub rsvd1: u16,
        pub ns_id: u32,
        pub cdw2: u32,
        pub cdw3: u32,
        pub metadata: u64,
        pub addr: u64,
        pub metadata_len: u32,
        pub data_len: u32,
        pub cdw10: u32,
        pub cdw11: u32,
        pub cdw12: u32,
        pub cdw13: u32,
        pub cdw14: u32,
        pub cdw15: u32,
        pub timeout_ms: u32,
        pub result: u32,
    }

    // #define NVME_IOCTL_ADMIN_CMD	_IOWR('N', 0x41, struct nvme_admin_cmd)
    const NVME_IOC_MAGIC: u8 = b'N';
    ioctl_readwrite!(nvme_ioctl_admin_cmd, NVME_IOC_MAGIC, 0x41, NvmeCmd);

    // #define nvme_admin_cmd nvme_passthru_cmd
    // #define NVME_IOCTL_IO_CMD	_IOWR('N', 0x43, struct nvme_passthru_cmd)
    ioctl_readwrite!(nvme_ioctl_io_cmd, NVME_IOC_MAGIC, 0x43, NvmeCmd);
}

#[derive(Copy, Clone, Debug, Default)]
struct NvmeCommand {
    flags: u8,
    ns_id: u32,
    cdw2: u32,
    cdw3: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
}

#[derive(Copy, Clone, Debug)]
enum Opcode {
    Admin(nvme_spec::AdminOpcode),
    Io(nvm::NvmOpcode),
}

fn nvme_command(
    file: &fs::File,
    opcode: Opcode,
    data: &mut (impl IntoBytes + FromBytes + ?Sized + Immutable + KnownLayout),
    command: &NvmeCommand,
) -> io::Result<u32> {
    let mut cmd = ioctl::NvmeCmd {
        opcode: match opcode {
            Opcode::Admin(nvme_spec::AdminOpcode(code)) | Opcode::Io(nvm::NvmOpcode(code)) => code,
        },
        flags: command.flags,
        rsvd1: 0,
        ns_id: command.ns_id,
        cdw2: command.cdw2,
        cdw3: command.cdw3,
        metadata: 0,
        addr: data.as_mut_bytes().as_mut_ptr() as u64,
        metadata_len: 0,
        data_len: data.as_mut_bytes().len() as u32,
        cdw10: command.cdw10,
        cdw11: command.cdw11,
        cdw12: command.cdw12,
        cdw13: command.cdw13,
        cdw14: command.cdw14,
        cdw15: command.cdw15,
        timeout_ms: command.timeout_ms,
        result: !0,
    };

    // SAFETY: The FD is owned by the corresponding File, and this IOCTL is
    //         legal to call on any valid FD. More documentation on this
    //         specific ioctl can be found in nvme_ioctl.h.
    let result = unsafe {
        match opcode {
            Opcode::Admin(_) => ioctl::nvme_ioctl_admin_cmd(file.as_raw_fd(), &mut cmd),
            Opcode::Io(_) => ioctl::nvme_ioctl_io_cmd(file.as_raw_fd(), &mut cmd),
        }
    };

    check_nvme_status(result?)?;
    Ok(cmd.result)
}

const PAGE_SIZE: usize = 4096;

#[repr(C, align(4096))]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct Page([u8; PAGE_SIZE]);

const ZERO_PAGE: Page = Page([0; PAGE_SIZE]);

#[bitfield(u32)]
pub struct InterruptCoalescing {
    pub aggregation_threshold: u8,
    pub aggregation_time: u8,
    reserved: u16,
}

pub fn nvme_get_features_interrupt_coalescing(file: &fs::File) -> io::Result<InterruptCoalescing> {
    let cmd = NvmeCommand {
        cdw10: nvme_spec::Cdw10SetFeatures::new()
            .with_fid(nvme_spec::Feature::INTERRUPT_COALESCING.0)
            .into(),
        ..Default::default()
    };
    let result = nvme_command(
        file,
        Opcode::Admin(nvme_spec::AdminOpcode::GET_FEATURES),
        &mut (),
        &cmd,
    )?;
    Ok(InterruptCoalescing::from(result))
}

pub fn nvme_set_features_interrupt_coalescing(
    file: &fs::File,
    coalescing: InterruptCoalescing,
) -> io::Result<()> {
    let cmd = NvmeCommand {
        cdw10: nvme_spec::Cdw10SetFeatures::new()
            .with_fid(nvme_spec::Feature::INTERRUPT_COALESCING.0)
            .into(),
        cdw11: coalescing.0,
        ..Default::default()
    };
    nvme_command(
        file,
        Opcode::Admin(nvme_spec::AdminOpcode::SET_FEATURES),
        &mut (),
        &cmd,
    )?;
    Ok(())
}

fn nvme_reservation_report(
    file: &fs::File,
    ns_id: u32,
    size: usize,
) -> io::Result<(
    nvm::ReservationReportExtended,
    Vec<nvm::RegisteredControllerExtended>,
)> {
    // One page should be good enough for most cases. Just in case let caller set bigger buffer size.
    let size = std::cmp::max(size, PAGE_SIZE);
    let mut buffer = vec![ZERO_PAGE; size.div_ceil(PAGE_SIZE)];
    let buffer = &mut buffer.as_mut_bytes()[..size];
    let cmd = NvmeCommand {
        ns_id,
        cdw10: nvm::Cdw10ReservationReport::new()
            .with_numd_z((size / 4 - 1) as u32)
            .into(),
        ..Default::default()
    };

    nvme_command(
        file,
        Opcode::Io(nvm::NvmOpcode::RESERVATION_REPORT),
        buffer,
        &cmd,
    )?;

    let report_header = nvm::ReservationReportExtended::read_from_prefix(&*buffer)
        .unwrap()
        .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    let mut controllers = Vec::new();

    // Return all controllers or none so caller can set correct buffer size and call again
    let controller_count = report_header.report.regctl.get() as usize;

    tracing::debug!(report_header = ?report_header, controller_count, "nvme_reservation_report");
    if controller_count > 0 {
        let mut source = size_of::<nvm::ReservationReportExtended>();
        let source_step = size_of::<nvm::RegisteredControllerExtended>();
        if source + controller_count * source_step <= size {
            for _i in 0..controller_count {
                let controller =
                    nvm::RegisteredControllerExtended::read_from_prefix(&buffer[source..])
                        .unwrap()
                        .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                tracing::debug!(controller = ?controller, "nvme_reservation_report");
                controllers.push(controller);
                source += source_step;
            }
        }
    }

    Ok((report_header, controllers))
}

fn call_nvme_reservation_report(
    file: &fs::File,
    ns_id: u32,
) -> io::Result<(
    nvm::ReservationReport,
    Vec<nvm::RegisteredControllerExtended>,
)> {
    // At least get the header
    let (mut report_header, mut controllers) =
        nvme_reservation_report(file, ns_id, size_of::<nvm::ReservationReportExtended>())?;

    // Missing controllers data
    let controller_count = report_header.report.regctl.get() as usize;
    if controller_count > 0 && controllers.is_empty() {
        // Call again with increased buffer size.
        let required_size = size_of::<nvm::ReservationReportExtended>()
            + controller_count * size_of::<nvm::RegisteredControllerExtended>();
        (report_header, controllers) = nvme_reservation_report(file, ns_id, required_size)?;
    }

    Ok((report_header.report, controllers))
}

pub fn nvme_identify_namespace_data(
    file: &fs::File,
    ns_id: u32,
) -> io::Result<nvm::IdentifyNamespace> {
    let size = size_of::<nvm::IdentifyNamespace>();
    let mut buffer = vec![ZERO_PAGE; size.div_ceil(PAGE_SIZE)];
    let buffer = &mut buffer.as_mut_bytes()[..size];
    let cmd = NvmeCommand {
        ns_id,
        cdw10: nvme_spec::Cdw10Identify::new()
            .with_cns(nvme_spec::Cns::NAMESPACE.0)
            .into(),
        ..Default::default()
    };

    nvme_command(
        file,
        Opcode::Admin(nvme_spec::AdminOpcode::IDENTIFY),
        buffer,
        &cmd,
    )?;

    let data = nvm::IdentifyNamespace::read_from_prefix(buffer).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    tracing::trace!(?data, "nvme_identify_namespace_data");
    Ok(data)
}

pub fn nvme_identify_controller_data(file: &fs::File) -> io::Result<nvme_spec::IdentifyController> {
    let size = size_of::<nvme_spec::IdentifyController>();
    let mut buffer = vec![ZERO_PAGE; size.div_ceil(PAGE_SIZE)];
    let buffer = &mut buffer.as_mut_bytes()[..size];
    let cmd = NvmeCommand {
        cdw10: nvme_spec::Cdw10Identify::new()
            .with_cns(nvme_spec::Cns::CONTROLLER.0)
            .into(),
        ..Default::default()
    };

    nvme_command(
        file,
        Opcode::Admin(nvme_spec::AdminOpcode::IDENTIFY),
        buffer,
        &cmd,
    )?;

    let data = nvme_spec::IdentifyController::read_from_prefix(buffer)
        .unwrap()
        .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    tracing::trace!(?data, "nvme_identify_controller_data");
    Ok(data)
}

pub fn check_nvme_status(status: i32) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        let errno = match nvme_spec::Status(status as u16) {
            nvme_spec::Status::RESERVATION_CONFLICT => libc::EBADE,
            status => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("nvme error {:#x?}", status),
                ))
            }
        };
        Err(io::Error::from_raw_os_error(errno))
    }
}

impl BlockDevice {
    pub(crate) async fn nvme_persistent_reservation_report(&self) -> io::Result<ReservationReport> {
        let file = self.file.clone();
        let DeviceType::NVMe { ns_id, .. } = self.device_type else {
            unreachable!("caller validated")
        };
        let (report_header, controllers) =
            unblock(move || call_nvme_reservation_report(&file, ns_id)).await?;

        from_nvme_reservation_report(&report_header, &controllers)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}
