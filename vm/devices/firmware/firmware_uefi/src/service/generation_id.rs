// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::UefiDevice;
use generation_id::GenerationId;

pub type GenerationIdServices = GenerationId;

impl UefiDevice {
    /// Update the low bits of the generation id pointer
    pub(crate) fn write_generation_id_low(&mut self, data: u32) {
        self.service.generation_id.write_generation_id_low(data)
    }

    /// Update the high bits of the generation id pointer
    pub(crate) fn write_generation_id_high(&mut self, data: u32) {
        self.service.generation_id.write_generation_id_high(data)
    }
}
