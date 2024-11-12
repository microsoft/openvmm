// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::NvramError;
use super::NvramResult;
use super::NvramSpecServices;
use guid::Guid;
use ucs2::Ucs2LeSlice;
use uefi_nvram_storage::InspectableNvramStorage;
use uefi_specs::uefi::common::EfiStatus;

/// Extension trait around `NvramServices` that makes it easier to use the API
/// outside the context of the UEFI device.
///
/// This trait provides various helper methods that make it easier to get/set
/// nvram variables without worrying about the nitty-gritty details of UCS-2
/// string encoding, pointer sizes/nullness, etc...
#[async_trait::async_trait]
pub trait NvramServicesExt {
    /// Get a variable identified by `name` (as a Rust string) + `vendor`,
    /// returning the variable's attributes and data.
    #[allow(dead_code)] // Useful for debugging
    async fn get_variable(
        &mut self,
        vendor: Guid,
        name: &str,
    ) -> Result<(u32, Vec<u8>), (EfiStatus, Option<NvramError>)>;

    /// Get a variable identified by `name` (as a UCS-2 string) + `vendor`,
    /// returning the variable's attributes and data.
    #[allow(dead_code)] // Useful for debugging
    async fn get_variable_ucs2(
        &mut self,
        vendor: Guid,
        name: &Ucs2LeSlice,
    ) -> Result<(u32, Vec<u8>), (EfiStatus, Option<NvramError>)>;

    /// Set a variable identified by `name` (as a Rust string) + `vendor` with
    /// the specified `attr` and `data`.
    async fn set_variable(
        &mut self,
        vendor: Guid,
        name: &str,
        attr: u32,
        data: Vec<u8>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)>;

    /// Set a variable identified by `name` (as a UCS-2 string) + `vendor` with
    /// the specified `attr` and `data`.
    async fn set_variable_ucs2(
        &mut self,
        vendor: Guid,
        name: &Ucs2LeSlice,
        attr: u32,
        data: Vec<u8>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)>;
}

#[async_trait::async_trait]
impl<S: InspectableNvramStorage> NvramServicesExt for NvramSpecServices<S> {
    async fn get_variable(
        &mut self,
        vendor: Guid,
        name: &str,
    ) -> Result<(u32, Vec<u8>), (EfiStatus, Option<NvramError>)> {
        let name = ucs2::Ucs2LeVec::from(name);
        self.get_variable_ucs2(vendor, &name).await
    }

    async fn get_variable_ucs2(
        &mut self,
        vendor: Guid,
        name: &Ucs2LeSlice,
    ) -> Result<(u32, Vec<u8>), (EfiStatus, Option<NvramError>)> {
        let mut attr = 0;
        // the low level UEFI APIs includes the `in_out_data_size` parameter so
        // that it can perform validation logic to ensure the user-provided
        // buffer is large enough to store the variable data.
        //
        // this validation logic isn't relevant to the high level API, as
        // consumers can simply use the returned Rust reference directly
        // (without having to copy it into another buffer).
        let mut in_out_data_size = u32::MAX;
        let NvramResult(data, status, err) = self
            .uefi_get_variable(
                Some(name.as_bytes()),
                vendor,
                &mut attr,
                &mut in_out_data_size,
                false,
            )
            .await;

        if matches!(status, EfiStatus::SUCCESS) {
            Ok((attr, data.expect("data will not be None on EFI_SUCCESS")))
        } else {
            Err((status, err))
        }
    }

    async fn set_variable(
        &mut self,
        vendor: Guid,
        name: &str,
        attr: u32,
        data: Vec<u8>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)> {
        let name = ucs2::Ucs2LeVec::from(name);
        self.set_variable_ucs2(vendor, &name, attr, data).await
    }

    async fn set_variable_ucs2(
        &mut self,
        vendor: Guid,
        name: &Ucs2LeSlice,
        attr: u32,
        data: Vec<u8>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)> {
        let NvramResult((), status, err) = self
            .uefi_set_variable(
                Some(name.as_bytes()),
                vendor,
                attr,
                data.len() as u32,
                Some(data),
            )
            .await;

        if matches!(status, EfiStatus::SUCCESS) {
            Ok(())
        } else {
            Err((status, err))
        }
    }
}
