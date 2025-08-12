use zerocopy::IntoBytes;
use thiserror::Error;
use crate::devices::tpm::protocol::{protocol::{SelfTestCmd, TpmCommand}, SessionTagEnum, TpmCommandError};

pub const TPM_DEVICE_MMIO_REGION_BASE_ADDRESS: u64 = 0xfed40000;
pub const TPM_DEVICE_MMIO_REGION_SIZE: u64 = 0x70;

pub const TPM_DEVICE_IO_PORT_RANGE_BEGIN: u16 = 0x1040;
pub const TPM_DEVICE_IO_PORT_RANGE_END: u16 = 0x1048;

pub const TPM_DEVICE_IO_PORT_CONTROL_OFFSET: u16 = 0;
pub const TPM_DEVICE_IO_PORT_DATA_OFFSET: u16 = 4;

pub const TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS: u64 =
    TPM_DEVICE_MMIO_REGION_BASE_ADDRESS + 0x80;
pub const TPM_DEVICE_MMIO_PORT_CONTROL: u64 =
    TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS + TPM_DEVICE_IO_PORT_CONTROL_OFFSET as u64;
pub const TPM_DEVICE_MMIO_PORT_DATA: u64 =
    TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS + TPM_DEVICE_IO_PORT_DATA_OFFSET as u64;
pub const TPM_DEVICE_MMIO_PORT_REGION_SIZE: u64 = 0x8;

pub struct Tpm<'a> {
    command_buffer: Option<&'a mut [u8]>,
    response_buffer: Option<&'a mut [u8]>,
}

impl<'a> Tpm<'a> {
    pub fn new() -> Tpm<'a> {
        Tpm {
            command_buffer: None,
            response_buffer: None,
        }
    }

    pub fn set_command_buffer(&mut self, buffer: &'a mut [u8]) {
        self.command_buffer = Some(buffer);
    }

    pub fn set_response_buffer(&mut self, buffer: &'a mut [u8]) {
        self.response_buffer = Some(buffer);
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_control_port(command: u32) -> u32 {
        let control_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN+TPM_DEVICE_IO_PORT_CONTROL_OFFSET;
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN+TPM_DEVICE_IO_PORT_DATA_OFFSET;
        super::io::outl(control_port, command);
        super::io::inl(data_port)
    }

    pub fn get_tcg_protocol_version() -> u32 {
        Tpm::get_control_port(64)
    }

    pub fn map_shared_memory(gpa: u32) -> u32 {
        let control_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN+TPM_DEVICE_IO_PORT_CONTROL_OFFSET;
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN+TPM_DEVICE_IO_PORT_DATA_OFFSET;
        super::io::outl(control_port, 0x1);
        super::io::outl(data_port, gpa);
        super::io::outl(control_port, 0x2);
        super::io::inl(data_port)
    }

    pub fn get_mapped_shared_memory() -> u32 {
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN+TPM_DEVICE_IO_PORT_DATA_OFFSET;
        Tpm::get_control_port(0x2);
        super::io::inl(data_port)
    }

    pub fn copy_to_command_buffer(&mut self, buffer: &[u8]) {
        self.command_buffer
        .as_mut()
        .unwrap()[..buffer.len()]
        .copy_from_slice(buffer);
    }

    pub fn copy_from_response_buffer(&mut self, buffer: &mut [u8]) {
        buffer.copy_from_slice(self.response_buffer.as_ref().unwrap());
    }

    pub fn execute_command() {
        let command_exec_mmio_addr = TPM_DEVICE_MMIO_REGION_BASE_ADDRESS + 0x4c;
        let command_exec_mmio_ptr = command_exec_mmio_addr as *mut u32;

        unsafe {
            *command_exec_mmio_ptr = 0x1;
        }

        while unsafe { *command_exec_mmio_ptr } == 0x1 {
            unsafe {
                core::arch::x86_64::_mm_pause();
            }
        }
    }

    pub fn run_command(&mut self, buffer: &[u8]) -> [u8; 4096] {
        assert!(buffer.len() <= 4096);
        self.copy_to_command_buffer(buffer);

        Tpm::execute_command();

        let mut response = [0; 4096];
        self.copy_from_response_buffer(&mut response);
        response
    }

    pub fn self_test(&mut self) -> Result<(), TpmCommandError> {
        let session_tag = SessionTagEnum::NoSessions;
        let cmd = SelfTestCmd::new(session_tag.into(), true);
        let response = self.run_command(cmd.as_bytes());
        
        match SelfTestCmd::base_validate_reply(&response, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error)),
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }
    
}

pub struct TpmUtil;
impl TpmUtil {
    pub fn get_self_test_cmd() -> [u8; 4096] {
        let session_tag = SessionTagEnum::NoSessions;
        let cmd = SelfTestCmd::new(session_tag.into(), true);
        let mut buffer = [0; 4096];
        buffer[..cmd.as_bytes().len()].copy_from_slice(cmd.as_bytes());
        buffer
    }
}