// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits and types for save/restore support.
//!
//! To implement save/restore for your device or state unit, you must do these
//! things:
//!
//! 1. You define a saved state type. This type needs to be stable across
//!    releases, so you should not share types with your implementation even if
//!    they are currently identical. By decoupling your runtime types and your
//!    saved state types, you are much less likely to accidentally break saved
//!    state compatibility.
//!
//! 2. `derive` some traits on your type. Specifically, you must derive
//!    [`Protobuf`] so that your type can be encoded into protobuf format, and
//!    any fields in your type must also implement [`Protobuf`].
//!
//!    So that your saved state type can show up in generated `.proto` files,
//!    you must also set an attribute `#[mesh(package = "my.package.name")]`.
//!    This specifies the protobuf package, as well as the file name of your
//!    protobuf file. For the root object of your device's saved state, this
//!    package name becomes part of the serialized state, so you cannot change
//!    it later.
//!
//!    Finally, in the root object of your saved state, you must additionally
//!    derive [`SavedStateRoot`]. This provides some additional metadata so that
//!    we can find your saved state type and generate a `.proto` file from it
//!    for analysis. You only need to put this on the root types or types that
//!    will be converted to [`SavedStateBlob`]; the infrastructure will find any
//!    dependent types. But it doesn't hurt to put it on other types, too.
//!
//! 3. Typically, you implement [`AsyncSaveRestore`]. This trait allows you to
//!    specify your associated saved state type and to implement `save` and
//!    `restore` methods that act on this type.
//!
//!    New code should use this trait even if it doesn't need to await anything
//!    yet. It will eventually replace [`SaveRestore`]. Existing synchronous
//!    implementations are adapted automatically and can migrate over time.
//!
//!    For some device types (such as vmbus devices), you may need to use a
//!    device-specific trait that provides additional parameters. But the
//!    pattern should be the same.

// UNSAFETY: Needed to use linkme for deriving SavedStateRoot.
#![expect(unsafe_code)]

/// Derives [`SavedStateRoot`] for a type.
///
/// This ensures that a saved state blob's metadata can be found, so that it can
/// be used to generate .proto files and perform offline analysis of saved state
/// compatibility.
///
/// To use this, you must also derive [`Protobuf`] and set a protobuf package
/// for your type. The package name should be defined to group related types
/// together; typically the same package should be used for all types defined in
/// a module.
///
/// For example:
///
/// ```rust
/// # use vmcore::save_restore::{SavedStateRoot, SavedStateBlob};
/// # use mesh::payload::Protobuf;
/// #[derive(Protobuf, SavedStateRoot)]
/// #[mesh(package = "test.my_device")]
/// struct MySavedState {
///     #[mesh(1)]
///     active: bool,
/// }
///
/// // This will now compile.
/// let _blob = SavedStateBlob::new(MySavedState { active: true });
/// ```
pub use save_restore_derive::SavedStateRoot;

use async_trait::async_trait;
use mesh::payload;
use mesh::payload::DefaultEncoding;
use mesh::payload::DescribedProtobuf;
use mesh::payload::Protobuf;
use mesh::payload::encoding::ImpossibleField;
use mesh::payload::message::ProtobufAny;
use mesh::payload::protofile::MessageDescription;
use std::future::Future;

/// Implemented by objects which can be saved/restored with an associated
/// type that can be serialized as a protobuf message.
///
/// New code should implement [`AsyncSaveRestore`] instead. This trait is kept
/// for existing implementations to migrate over time and will eventually be
/// removed.
pub trait SaveRestore {
    /// The concrete saved state type.
    type SavedState;

    /// Saves the object's state.
    fn save(&mut self) -> Result<Self::SavedState, SaveError>;
    /// Restores the object's state.
    ///
    /// The destination must be stopped and newly constructed or successfully
    /// reset. Restore does not support overwriting previously running or
    /// restored state without a reset.
    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError>;
}

/// Asynchronous save/restore with a concrete saved state type.
///
/// Use this trait for new code, even when no asynchronous work is needed yet.
/// It will eventually be the only typed save/restore interface.
///
/// Types implementing [`SaveRestore`] are adapted automatically. To migrate a
/// type to native asynchronous save/restore, replace its `SaveRestore`
/// implementation with this trait; the blanket implementation prevents
/// implementing both on the same type.
pub trait AsyncSaveRestore {
    /// The concrete saved state type.
    type SavedState: Send;

    /// Saves the object's state.
    fn save(&mut self) -> impl Send + Future<Output = Result<Self::SavedState, SaveError>>;
    /// Restores the object's state.
    ///
    /// The destination must be stopped and newly constructed or successfully
    /// reset. After a restore attempt, reset before attempting another restore.
    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> impl Send + Future<Output = Result<(), RestoreError>>;
}

impl<T: SaveRestore + Send> AsyncSaveRestore for T
where
    T::SavedState: Send,
{
    type SavedState = T::SavedState;

    async fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        SaveRestore::save(self)
    }

    async fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        SaveRestore::restore(self, state)
    }
}

/// Convenience type for objects that do not support being saved.
pub enum SavedStateNotSupported {}

impl DefaultEncoding for SavedStateNotSupported {
    type Encoding = ImpossibleField;
}

impl SavedStateRoot for SavedStateNotSupported {
    // This type should not be included in the .proto output.
    fn do_not_impl_this_manually(&self) {}
}

/// Convenience type for objects that have no saved state.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "save_restore")]
pub struct NoSavedState;

/// Trait implemented by objects that implement `SaveRestore`
pub trait ProtobufSaveRestore {
    /// Save the object.
    fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    /// Restore the object.
    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
}

/// Object-safe asynchronous save/restore using protobuf-encoded state.
///
/// Implement [`AsyncSaveRestore`] for typed state; its protobuf encoding and
/// decoding are provided automatically. Implement this trait directly only
/// when the state is already an opaque blob, such as for a remote device.
#[async_trait]
pub trait AsyncProtobufSaveRestore: Send {
    /// Saves the object's state.
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    /// Restores the object's state.
    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
}

/// An opaque saved state blob, encoded as a protobuf message.
#[derive(Debug, Protobuf)]
#[mesh(transparent)]
pub struct SavedStateBlob(ProtobufAny);

/// Trait implemented by "root" saved state blobs, which are ones that either
/// form the root of a saved state tree, or at points in the tree where the type
/// is not known at compile time.
///
/// **Do not implement this trait manually.** Derive it with
/// `#[derive(SavedStateRoot)]`. This emits extra code to ensure that the saved
/// state type metadata is included in the binary.
pub trait SavedStateRoot: DescribedProtobuf {
    #[doc(hidden)]
    fn do_not_impl_this_manually(&self);
}

impl SavedStateBlob {
    /// Encodes `data` as a protobuf message.
    pub fn new<T: SavedStateRoot>(data: T) -> Self {
        Self(ProtobufAny::new(data))
    }

    /// Decodes the protobuf message into `T`.
    pub fn parse<T: SavedStateRoot>(&self) -> Result<T, payload::Error> {
        self.0.parse()
    }
}

impl<T: SaveRestore> ProtobufSaveRestore for T
where
    T::SavedState: 'static + Send + SavedStateRoot,
{
    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        SaveRestore::save(self).map(SavedStateBlob::new)
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        SaveRestore::restore(self, state.parse()?)
    }
}

#[async_trait]
impl<T: AsyncSaveRestore + Send> AsyncProtobufSaveRestore for T
where
    T::SavedState: 'static + SavedStateRoot,
{
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        AsyncSaveRestore::save(self).await.map(SavedStateBlob::new)
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        AsyncSaveRestore::restore(self, state.parse()?).await
    }
}

/// A restore error.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// unknown entry ID
    #[error("unknown entry id: {0}")]
    UnknownEntryId(String),
    /// restore failure in a child object
    #[error("failed to restore child device {0}")]
    ChildError(String, #[source] Box<RestoreError>),
    /// failure to decode a protobuf object
    #[error("failed to decode protobuf")]
    ProtobufDecode(#[from] payload::Error),
    /// this object does not support save state
    #[error("unexpected saved state")]
    SavedStateNotSupported,
    /// custom saved state corruption error
    #[error("saved state is invalid")]
    InvalidSavedState(#[source] anyhow::Error),
    /// non-state-related restore failure
    #[error(transparent)]
    Other(anyhow::Error),
}

/// A save error.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// This object does not support saved state.
    #[error("save state not supported")]
    NotSupported,
    /// Save failed in child object.
    #[error("failed to save child device {0}")]
    ChildError(String, #[source] Box<SaveError>),
    /// Save failed due to some other error.
    #[error(transparent)]
    Other(anyhow::Error),
    /// The child saved state is invalid.
    #[error("child saved state is invalid")]
    InvalidChildSavedState(#[source] anyhow::Error),
}

/// Gets the message descriptions for all types deriving [`SavedStateRoot`].
///
/// This can be used with
/// [`DescriptorWriter`](mesh::payload::protofile::DescriptorWriter) to write
/// `.proto` files for the saved states.
pub fn saved_state_roots() -> impl Iterator<Item = &'static MessageDescription<'static>> {
    private::SAVED_STATE_ROOTS.iter().flatten().copied()
}

// For `save_restore_derive`
#[doc(hidden)]
pub mod private {
    pub use linkme;
    pub use mesh::payload::protofile;

    // Use Option<&X> in case the linker inserts some stray nulls, as we think
    // it might on Windows.
    //
    // See <https://devblogs.microsoft.com/oldnewthing/20181108-00/?p=100165>.
    #[linkme::distributed_slice]
    pub static SAVED_STATE_ROOTS: [Option<&'static protofile::MessageDescription<'static>>] = [..];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(SAVED_STATE_ROOTS)]
    static WORKAROUND: Option<&'static protofile::MessageDescription<'static>> = None;

    #[doc(hidden)]
    #[macro_export]
    macro_rules! declare_saved_state_root {
        ($ident:ty) => {
            impl $crate::save_restore::SavedStateRoot for $ident {
                fn do_not_impl_this_manually(&self) {}
            }
            const _: () = {
                use $crate::save_restore::private::SAVED_STATE_ROOTS;
                use $crate::save_restore::private::linkme;
                use $crate::save_restore::private::protofile;

                #[linkme::distributed_slice(SAVED_STATE_ROOTS)]
                #[linkme(crate = linkme)]
                static DESCRIPTION: Option<&'static protofile::MessageDescription<'static>> =
                    Some(&protofile::message_description::<$ident>());
            };
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use test_with_tracing::test;

    #[derive(Debug, Protobuf, SavedStateRoot)]
    #[mesh(package = "test.save_restore")]
    struct State {
        #[mesh(1)]
        value: u32,
    }

    struct Device(u32);

    impl SaveRestore for Device {
        type SavedState = State;

        fn save(&mut self) -> Result<State, SaveError> {
            if self.0 == u32::MAX {
                return Err(SaveError::NotSupported);
            }
            Ok(State { value: self.0 })
        }

        fn restore(&mut self, state: State) -> Result<(), RestoreError> {
            if state.value == u32::MAX {
                return Err(RestoreError::SavedStateNotSupported);
            }
            self.0 = state.value;
            Ok(())
        }
    }

    #[test]
    fn synchronous_state_is_compatible_with_async_adapter() {
        block_on(async {
            let mut device = Device(42);
            let legacy = ProtobufSaveRestore::save(&mut device).unwrap();
            let asynchronous = AsyncProtobufSaveRestore::save(&mut device).await.unwrap();
            assert_eq!(payload::encode(legacy), payload::encode(asynchronous));

            let state = ProtobufSaveRestore::save(&mut device).unwrap();
            let mut restored = Device(0);
            let erased: &mut dyn AsyncProtobufSaveRestore = &mut restored;
            erased.restore(state).await.unwrap();
            let state = erased.save().await.unwrap();
            assert_eq!(state.parse::<State>().unwrap().value, 42);
            ProtobufSaveRestore::restore(&mut device, state).unwrap();
            assert_eq!(device.0, 42);
        });
    }

    #[test]
    fn async_adapter_preserves_errors() {
        block_on(async {
            assert!(matches!(
                AsyncProtobufSaveRestore::save(&mut Device(u32::MAX)).await,
                Err(SaveError::NotSupported)
            ));

            let mut device = Device(7);
            assert!(matches!(
                AsyncProtobufSaveRestore::restore(
                    &mut device,
                    SavedStateBlob::new(State { value: u32::MAX })
                )
                .await,
                Err(RestoreError::SavedStateNotSupported)
            ));
            assert!(matches!(
                AsyncProtobufSaveRestore::restore(&mut device, SavedStateBlob::new(NoSavedState))
                    .await,
                Err(RestoreError::ProtobufDecode(_))
            ));
            assert_eq!(device.0, 7);
        });
    }
}
