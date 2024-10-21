// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to define VM state.

pub(crate) use macros::state_trait;

use hvdef::HvRegisterValue;
use inspect::Inspect;
use std::fmt::Debug;
use thiserror::Error;

pub trait StateElement<C, V>: Sized + Inspect {
    /// Returns whether this state is present for the partition, based on the
    /// partition capabilities.
    fn is_present(caps: &C) -> bool;

    /// Returns the value at VM reset.
    fn at_reset(caps: &C, vp: &V) -> Self;

    /// Returns whether it's possible to read this value and compare it to an
    /// expected value.
    ///
    /// This will be false when the value may change as soon as its set (e.g., a
    /// timestamp counter for hypervisors that cannot freeze time).
    fn can_compare(_caps: &C) -> bool {
        true
    }
}

pub trait HvRegisterState<T, const COUNT: usize>: Default {
    fn names(&self) -> &'static [T; COUNT];
    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>);
    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>);
}

#[derive(Debug, Error)]
#[error("state access error, phase {phase}")]
pub struct StateError<T: 'static + Debug + std::error::Error> {
    pub(crate) phase: &'static str,
    #[source]
    pub(crate) err: T,
}

mod macros {
    /// Generates a trait for getting and setting some aspect of partition state
    /// (e.g. per-partition state, per-VP state, per-VTL state).
    ///
    /// The trait is made up of methods for getting and setting individual
    /// pieces of state. Each piece can be individually present or missing for a
    /// partition, e.g. because certain processor features are enabled or not.
    ///
    /// A function is generated that will reset each individual piece of state.
    ///
    /// Another generated function will validate that each piece of state is
    /// equal to its reset state (which is useful to validate that a partition's
    /// initial state is equal to its reset state).
    ///
    /// The intent is that the partition types implement these traits to get and
    /// set individual pieces of state.
    macro_rules! state_trait {
        ($doc:tt, $trait:ident, $caps:ty, $vp:ty, $save_state:ident, $package:expr,
            $(($field_num:literal, $id:expr, $get:ident, $set:ident, $ty:ty $(,)?)),* $(,)?
        ) => {
            #[doc = $doc]
            pub trait $trait {
                type Error: 'static + std::error::Error + Send + Sync;

                /// Gets the partition's capabilities.
                fn caps(&self) -> &$caps;

                /// Commits any state changes made with the `set_*` methods.
                fn commit(&mut self) -> Result<(), Self::Error>;

                $(
                /// Gets the specified state.
                fn $get(&mut self) -> Result<$ty, Self::Error>;
                /// Sets the specified state.
                fn $set(&mut self, value: &$ty) -> Result<(), Self::Error>;
                )*

                /// Save all state that can be restored by a call to restore.
                fn save_all(&mut self) -> Result<$save_state, $crate::state::StateError<Self::Error>> {
                    let mut save_state = $save_state::default();
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            save_state.$get = Some(self.$get().map_err(|err| $crate::state::StateError{phase: concat!("save ", stringify!($id)), err})?);
                        }
                    )*

                    Ok(save_state)
                }

                /// Restore state elements saved in save state.
                fn restore_all(&mut self, state: &$save_state) -> Result<(), $crate::state::StateError<Self::Error>> {
                    $(
                        if let Some(value) = state.$get.as_ref() {
                            // TODO: assert good or not?
                            assert!(<$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()));
                            self.$set(value).map_err(|err| $crate::state::StateError{phase: concat!("restore ", stringify!($id)), err})?;
                            if cfg!(debug_assertions) {
                                if <$ty as $crate::state::StateElement<$caps, $vp>>::can_compare(self.caps()) {
                                    assert_eq!(&self.$get().expect($id), value, "restore state mismatch (actual/expected)");
                                }
                            }
                        }
                    )*

                    self.commit().map_err(|err| $crate::state::StateError{phase: "commit restore", err})
                }

                /// Resets all the state elements to their initial state (after machine reset).
                fn reset_all(&mut self, vp_info: &$vp) -> Result<(), $crate::state::StateError<Self::Error>> {
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            self.$set(&<$ty as $crate::state::StateElement<$caps, $vp>>::at_reset(self.caps(), vp_info)).map_err(|err| $crate::state::StateError{phase: concat!("reset ", stringify!($id)), err})?;
                        }
                    )*

                    if cfg!(debug_assertions) {
                        self.check_reset_all(vp_info);
                    }
                    Ok(())
                }

                /// Validates that all state elements are in their initial state (after machine reset).
                fn check_reset_all(&mut self, vp_info: &$vp) {
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::can_compare(self.caps()) && <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            assert_eq!(self.$get().expect($id), <$ty as $crate::state::StateElement<$caps, $vp>>::at_reset(self.caps(), vp_info), "reset state mismatch (actual/expected)");
                        }
                    )*
                }

                fn inspect_all(&mut self, req: ::inspect::Request<'_>) {
                    let mut resp = req.respond();
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            resp.field_with($id, || self.$get().ok());
                        }
                    )*
                }
            }

            /// Saved state type that can be used to save or restore.
            #[derive(Debug, Default, PartialEq, Eq, mesh_protobuf::Protobuf, vmcore::save_restore::SavedStateRoot)]
            #[mesh(package = $package)]
            pub struct $save_state {
                $(
                    #[mesh($field_num)]
                    $get: Option<$ty>,
                )*
            }
        };
    }

    pub(crate) use state_trait;
}
