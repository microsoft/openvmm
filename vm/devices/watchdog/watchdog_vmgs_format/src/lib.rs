// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The watchdog state VMGS data format, as used in Hyper-V.
//!
//! At the moment, the format is dead simple: it's just a single byte - either a
//! 1 or a 0 - tha represents if the previous boot failed.

#![forbid(unsafe_code)]

use thiserror::Error;
use vmcore::non_volatile_store::NonVolatileStore;

/// Watchdog VMGS formatted buffer contained invalid data.
#[derive(Debug, Error)]
#[error("expected single byte (0 or 1), got {0} bytes, starting with {1:?}")]
pub struct InvalidFormatError(usize, Option<u8>);

/// Data format used to persist watchdog state to VMGS.
struct WatchdogVmgsFormat {
    boot_failure: bool,
}

impl WatchdogVmgsFormat {
    /// Return a new instance [`WatchdogVmgsFormat`].
    fn new() -> Self {
        Self {
            boot_failure: false,
        }
    }

    /// Update existing existing instance of [`WatchdogVmgsFormat`] with the data
    /// stored in the provided buffer.
    fn update_from_slice(&mut self, buf: &[u8]) -> Result<(), InvalidFormatError> {
        let boot_status = match buf {
            [] => return Err(InvalidFormatError(0, None)),
            [0] => false,
            [1] => true,
            [other, ..] => return Err(InvalidFormatError(buf.len(), Some(*other))),
        };

        self.boot_failure = boot_status;

        Ok(())
    }

    /// Return a slice to persist to VMGS.
    fn as_slice(&self) -> &[u8] {
        if self.boot_failure {
            &[1]
        } else {
            &[0]
        }
    }
}

/// Errors which may occur as part of [`WatchdogVmgsFormatStore`]
/// operations.
#[derive(Debug, Error)]
#[expect(missing_docs)] // self-explanatory variants
pub enum WatchdogVmgsFormatStoreError {
    #[error("could not access non-volatile store")]
    NonVolatileStoreAccessError(#[source] vmcore::non_volatile_store::NonVolatileStoreError),
    #[error("invalid data pull from non-volatile store")]
    InvalidFormat(#[source] InvalidFormatError),
}

/// Persist and restore watchdog data into a [`NonVolatileStore`] using the VMGS
/// watchdog data format.
pub struct WatchdogVmgsFormatStore {
    store: Box<dyn NonVolatileStore>,
    state: WatchdogVmgsFormat,
}

impl WatchdogVmgsFormatStore {
    /// Construct a new instance of [`WatchdogVmgsFormatStore`], populated
    /// with data from the provided store.
    pub async fn new(
        mut store: Box<dyn NonVolatileStore>,
    ) -> Result<Self, WatchdogVmgsFormatStoreError> {
        use WatchdogVmgsFormatStoreError as Error;

        let buf = store
            .restore()
            .await
            .map_err(Error::NonVolatileStoreAccessError)?;

        let mut state = WatchdogVmgsFormat::new();

        if let Some(buf) = buf {
            state
                .update_from_slice(&buf)
                .map_err(Error::InvalidFormat)?;
        }

        Ok(Self { store, state })
    }

    async fn flush(&mut self) -> Result<(), WatchdogVmgsFormatStoreError> {
        use WatchdogVmgsFormatStoreError as Error;

        self.store
            .persist(self.state.as_slice().to_vec())
            .await
            .map_err(Error::NonVolatileStoreAccessError)?;

        Ok(())
    }

    /// Enable the boot status flag.
    pub async fn set_boot_failure(&mut self) -> Result<(), WatchdogVmgsFormatStoreError> {
        let prev = self.state.boot_failure;
        self.state.boot_failure = true;

        // only flush if data has changed
        if !prev {
            self.flush().await?;
        }

        Ok(())
    }

    /// Read the boot status flag, returning it's value, while simultaneously
    /// resetting it.
    pub async fn read_and_clear_boot_status(
        &mut self,
    ) -> Result<bool, WatchdogVmgsFormatStoreError> {
        let prev = self.state.boot_failure;
        self.state.boot_failure = false;

        // only flush if data has changed
        if prev {
            self.flush().await?;
        }

        Ok(prev)
    }
}
