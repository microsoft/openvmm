// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific implementation of TPM device access.

#![expect(dead_code)]

use crate::devices::tpm::TpmDevice;

const TPM_DEVICE_MMIO_REGION_BASE_ADDRESS: u64 = 0xfed40000;
const TPM_DEVICE_MMIO_REGION_SIZE: u64 = 0x70;

const TPM_DEVICE_IO_PORT_RANGE_BEGIN: u16 = 0x1040;
const TPM_DEVICE_IO_PORT_RANGE_END: u16 = 0x1048;

const TPM_DEVICE_IO_PORT_CONTROL_OFFSET: u16 = 0;
const TPM_DEVICE_IO_PORT_DATA_OFFSET: u16 = 4;

const TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS: u64 = TPM_DEVICE_MMIO_REGION_BASE_ADDRESS + 0x80;
const TPM_DEVICE_MMIO_PORT_CONTROL: u64 =
    TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS + TPM_DEVICE_IO_PORT_CONTROL_OFFSET as u64;
const TPM_DEVICE_MMIO_PORT_DATA: u64 =
    TPM_DEVICE_MMIO_PORT_REGION_BASE_ADDRESS + TPM_DEVICE_IO_PORT_DATA_OFFSET as u64;
const TPM_DEVICE_MMIO_PORT_REGION_SIZE: u64 = 0x8;

/// Represents a TPM device accessible via MMIO and IO ports.
/// This struct provides methods to interact with the TPM device
/// using the TpmDevice trait.
pub struct Tpm<'a> {
    command_buffer: Option<&'a mut [u8]>,
    response_buffer: Option<&'a mut [u8]>,
}

impl<'a> Tpm<'a> {
    /// Creates a new TpmDevice instance.
    pub fn new() -> Self {
        Tpm {
            command_buffer: None,
            response_buffer: None,
        }
    }

    fn get_control_port(command: u32) -> u32 {
        let control_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN + TPM_DEVICE_IO_PORT_CONTROL_OFFSET;
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN + TPM_DEVICE_IO_PORT_DATA_OFFSET;
        super::io::outl(control_port, command);
        super::io::inl(data_port)
    }
}

impl<'a> TpmDevice<'a> for Tpm<'a> {
    fn set_command_buffer(&mut self, buffer: &'a mut [u8]) {
        self.command_buffer = Some(buffer);
    }

    fn set_response_buffer(&mut self, buffer: &'a mut [u8]) {
        self.response_buffer = Some(buffer);
    }

    fn get_tcg_protocol_version() -> u32 {
        Tpm::get_control_port(64)
    }

    fn get_mapped_shared_memory() -> u32 {
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN + TPM_DEVICE_IO_PORT_DATA_OFFSET;
        Tpm::get_control_port(0x2);
        super::io::inl(data_port)
    }

    fn map_shared_memory(gpa: u32) -> u32 {
        let control_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN + TPM_DEVICE_IO_PORT_CONTROL_OFFSET;
        let data_port = TPM_DEVICE_IO_PORT_RANGE_BEGIN + TPM_DEVICE_IO_PORT_DATA_OFFSET;
        super::io::outl(control_port, 0x1);
        super::io::outl(data_port, gpa);
        super::io::outl(control_port, 0x2);
        super::io::inl(data_port)
    }

    fn submit_command(&mut self, buffer: &[u8]) -> [u8; 4096] {
        assert!(buffer.len() <= 4096);
        self.copy_to_command_buffer(buffer);

        Self::execute_command_no_check();

        let mut response = [0; 4096];
        self.copy_from_response_buffer(&mut response);
        response
    }

    #[expect(
        clippy::while_immutable_condition,
        reason = "tpm device updates status of MMIO read"
    )]
    fn execute_command_no_check() {
        let command_exec_mmio_addr = TPM_DEVICE_MMIO_REGION_BASE_ADDRESS + 0x4c;
        let command_exec_mmio_ptr = command_exec_mmio_addr as *mut u32;

        // SAFETY: we are writing to a valid memory-mapped IO register.
        unsafe {
            *command_exec_mmio_ptr = 0x1;
        }

        // SAFETY: we are reading from a valid memory-mapped IO register.
        while unsafe { *command_exec_mmio_ptr } == 0x1 {
            core::hint::spin_loop();
        }
    }

    fn copy_to_command_buffer(&mut self, buffer: &[u8]) {
        assert!(buffer.len() <= 4096);
        self.command_buffer.as_mut().unwrap()[..buffer.len()].copy_from_slice(buffer);
    }

    fn copy_from_response_buffer(&self, buffer: &mut [u8]) {
        assert!(buffer.len() <= 4096);
        buffer.copy_from_slice(self.response_buffer.as_ref().unwrap());
    }
}
