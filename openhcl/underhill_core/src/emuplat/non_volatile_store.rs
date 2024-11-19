// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Non-volatile store backed by the VMGS broker.

use vmcore::non_volatile_store::NonVolatileStore;
use vmgs_broker::non_volatile_store::EncryptionNotSupported;
use vmgs_broker::non_volatile_store::VmgsNonVolatileStore;

/// An API for interacting with VMGS as an opaque [`NonVolatileStore`]
pub trait VmbsBrokerNonVolatileStore {
    /// Return a new [`NonVolatileStore`] object backed by a particular VMGS
    /// file-id.
    fn as_non_volatile_store(
        &self,
        file_id: vmgs::FileId,
        encrypted: bool,
    ) -> Result<Box<dyn NonVolatileStore>, EncryptionNotSupported>;
}

impl VmbsBrokerNonVolatileStore for vmgs_broker::VmgsClient {
    fn as_non_volatile_store(
        &self,
        file_id: vmgs::FileId,
        encrypted: bool,
    ) -> Result<Box<dyn NonVolatileStore>, EncryptionNotSupported> {
        Ok(Box::new(VmgsNonVolatileStore::new(
            self.clone(),
            file_id,
            encrypted,
        )?))
    }
}
