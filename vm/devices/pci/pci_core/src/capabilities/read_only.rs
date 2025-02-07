// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A generic, read-only PCI Capability (backed by an [`IntoBytes`] type).

use super::PciCapability;
use inspect::Inspect;
use std::fmt::Debug;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Helper to define a read-only [`PciCapability`] from an [`IntoBytes`] type.
#[derive(Debug)]
pub struct ReadOnlyCapability<T> {
    label: String,
    data: T,
}

impl<T: IntoBytes + Immutable + KnownLayout> ReadOnlyCapability<T> {
    /// Create a new [`ReadOnlyCapability`]
    pub fn new(label: impl Into<String>, data: T) -> Self {
        Self {
            label: label.into(),
            data,
        }
    }
}

impl<T: Debug> Inspect for ReadOnlyCapability<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("label", &self.label)
            .display_debug("data", &self.data);
    }
}

impl<T> PciCapability for ReadOnlyCapability<T>
where
    T: IntoBytes + Send + Sync + Debug + Immutable + KnownLayout,
{
    fn label(&self) -> &str {
        &self.label
    }

    fn len(&self) -> usize {
        size_of::<T>()
    }

    fn read_u32(&self, offset: u16) -> u32 {
        if offset as usize + 4 <= self.len() {
            let offset = offset.into();
            u32::from_ne_bytes(self.data.as_bytes()[offset..offset + 4].try_into().unwrap())
        } else {
            !0
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        tracelimit::warn_ratelimited!(
            label = ?self.label,
            ?offset,
            ?val,
            "write to read-only capability"
        );
    }

    fn reset(&mut self) {}
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    // This is a noop impl, as the capability is (by definition) read only.
    impl<T> SaveRestore for ReadOnlyCapability<T> {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
