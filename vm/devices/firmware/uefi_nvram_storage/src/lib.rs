// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Abstractions to support pluggable UEFI nvram storage backends (e.g: in memory, file backed, etc...)

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub use uefi_specs::uefi::time::EFI_TIME;

pub mod in_memory;

use guid::Guid;
#[cfg(feature = "inspect")]
pub use inspect_ext::InspectableNvramStorage;
use std::fmt::Debug;
use thiserror::Error;
use ucs2::Ucs2LeSlice;
use ucs2::Ucs2LeVec;

#[derive(Debug, Error)]
pub enum NvramStorageError {
    #[error("error deserializing nvram storage")]
    Deserialize,
    #[error("error loading data from Nvram storage")]
    Load(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("error committing data to Nvram storage")]
    Commit(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("nvram is out of space")]
    OutOfSpace,
    #[error("variable name too long")]
    VariableNameTooLong,
    #[error("variable data too long")]
    VariableDataTooLong,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NextVariable {
    InvalidKey,
    EndOfList,
    Exists {
        name: Ucs2LeVec,
        vendor: Guid,
        attr: u32,
    },
}

/// Abstraction over persistent nvram variable storage (e.g: in-memory,
/// file-backed, vmgs-backed, etc.).
///
/// Implementors of this interface are **not required** to perform attribute
/// validation, and should simply store/retrieve data.
#[async_trait::async_trait]
pub trait NvramStorage: Send + Sync {
    /// Return the `attr` + `data` of the variable identified by `name` +
    /// `vendor`.
    async fn get_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError>;

    /// Set the value of variable identified by `name` + `vendor` to the
    /// provided `attr` + `data`.
    ///
    /// This method will persist any modifications to a backing data store.
    async fn set_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        attr: u32,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<(), NvramStorageError>;

    /// Append data to a variable identified by `name` + `vendor` from the Nvram
    /// storage.
    ///
    /// Returns `true` if the variable was appended to, or `false` if it could
    /// not be found.
    ///
    /// This method will persist any modifications to a backing data store.
    async fn append_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<bool, NvramStorageError>;

    /// Remove a variable identified by `name` + `vendor` from the Nvram
    /// storage.
    ///
    /// Returns `true` if the variable was removed, or `false` if it could not
    /// be found.
    ///
    /// This method will persist any modifications to a backing data store.
    async fn remove_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<bool, NvramStorageError>;

    /// Return the variable key immediately after the variable identified by
    /// `name` + `vendor`. If `name_vendor` is `None`, return the first
    /// variable.
    async fn next_variable(
        &mut self,
        name_vendor: Option<(&Ucs2LeSlice, Guid)>,
    ) -> Result<NextVariable, NvramStorageError>;

    /// Return `true` if the underlying store doesn't contain any vars
    async fn is_empty(&mut self) -> Result<bool, NvramStorageError> {
        Ok(matches!(
            self.next_variable(None).await?,
            NextVariable::EndOfList
        ))
    }
}

#[async_trait::async_trait]
impl NvramStorage for Box<dyn NvramStorage> {
    async fn get_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError> {
        (**self).get_variable(name, vendor).await
    }

    async fn set_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        attr: u32,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<(), NvramStorageError> {
        (**self)
            .set_variable(name, vendor, attr, data, timestamp)
            .await
    }

    async fn append_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<bool, NvramStorageError> {
        (**self)
            .append_variable(name, vendor, data, timestamp)
            .await
    }

    async fn remove_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<bool, NvramStorageError> {
        (**self).remove_variable(name, vendor).await
    }

    async fn next_variable(
        &mut self,
        name_vendor: Option<(&Ucs2LeSlice, Guid)>,
    ) -> Result<NextVariable, NvramStorageError> {
        (**self).next_variable(name_vendor).await
    }
}

/// Defines a trait that combines NvramStorage and Inspect
#[cfg(feature = "inspect")]
mod inspect_ext {
    use super::*;
    use inspect::Inspect;

    /// Extends [`NvramStorage`] with a bound on [`Inspect`]
    pub trait InspectableNvramStorage: NvramStorage + Inspect {}
    impl<T: NvramStorage + Inspect> InspectableNvramStorage for T {}

    #[async_trait::async_trait]
    impl NvramStorage for Box<dyn InspectableNvramStorage> {
        async fn get_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
        ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError> {
            (**self).get_variable(name, vendor).await
        }

        async fn set_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
            attr: u32,
            data: Vec<u8>,
            timestamp: EFI_TIME,
        ) -> Result<(), NvramStorageError> {
            (**self)
                .set_variable(name, vendor, attr, data, timestamp)
                .await
        }

        async fn append_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
            data: Vec<u8>,
            timestamp: EFI_TIME,
        ) -> Result<bool, NvramStorageError> {
            (**self)
                .append_variable(name, vendor, data, timestamp)
                .await
        }

        async fn remove_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
        ) -> Result<bool, NvramStorageError> {
            (**self).remove_variable(name, vendor).await
        }

        async fn next_variable(
            &mut self,
            name_vendor: Option<(&Ucs2LeSlice, Guid)>,
        ) -> Result<NextVariable, NvramStorageError> {
            (**self).next_variable(name_vendor).await
        }
    }
}
