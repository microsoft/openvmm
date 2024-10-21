// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crypto types defined in `BiosInterface.h`

use crate::UefiDevice;
use guestmem::GuestMemoryError;
use uefi_specs::hyperv::crypto::CryptoCommandDescriptor;
use uefi_specs::uefi::common::EfiStatus;

impl UefiDevice {
    pub(crate) fn crypto_handle_command(&mut self, desc_addr: u64) {
        let mut desc: CryptoCommandDescriptor = match self.gm.read_plain(desc_addr) {
            Ok(desc) => desc,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Could not read CryptoCommandDescriptor from guest memory",
                );
                return;
            }
        };

        let status = match self.crypto_handle_command_inner(desc_addr, desc) {
            Ok(status) => status,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Guest memory error while handling crypto command",
                );
                EfiStatus::DEVICE_ERROR
            }
        };
        desc.status = status.into();

        if let Err(err) = self.gm.write_plain(desc_addr, &desc) {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                "Could not write CryptoCommandDescriptor into guest memory",
            );
        }
    }

    fn crypto_handle_command_inner(
        &mut self,
        desc_addr: u64,
        desc: CryptoCommandDescriptor,
    ) -> Result<EfiStatus, GuestMemoryError> {
        use uefi_specs::hyperv::crypto::CryptoCommand;

        let command_addr = desc_addr + size_of_val(&desc) as u64;

        match desc.command {
            CryptoCommand::GET_RANDOM_NUMBER => {
                use uefi_specs::hyperv::crypto::CryptoGetRandomNumberParams;
                // Our current UEFI implementation should never ask for more than 64 bits (8 bytes) at a time
                // Larger guest requests will be divided into 8-byte chunks by the firmware.
                const MAXIMUM_RNG_SIZE: usize = 8;

                let command: CryptoGetRandomNumberParams = self.gm.read_plain(command_addr)?;
                let buffer_size = command.buffer_size as usize;

                if buffer_size > MAXIMUM_RNG_SIZE {
                    return Ok(EfiStatus::INVALID_PARAMETER);
                }

                let random_number = &mut [0; MAXIMUM_RNG_SIZE][..buffer_size];
                getrandom::getrandom(random_number).expect("rng failure");

                self.gm
                    .write_at(command.buffer_address.into(), random_number)?;

                Ok(EfiStatus::SUCCESS)
            }
            command => {
                tracelimit::warn_ratelimited!(?command, "unknown or unhandled crypto command");
                Ok(EfiStatus::DEVICE_ERROR)
            }
        }
    }
}
