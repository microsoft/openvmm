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
    /// Returns the registers that hold this state, in the order in which
    /// `get_values` and `set_values` pair them with values.
    fn names(&self) -> &'static [T; COUNT];
    /// Writes this state's value of each register that `names` lists, in that
    /// order, to `it`.
    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>);
    /// Sets this state from the value of each register that `names` lists, in
    /// that order, which `it` yields.
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
    /// A backend can refuse a save of a state that its pieces cannot hold by
    /// overriding `check_saveable`, which the generated `save_all` calls first.
    ///
    /// The intent is that the partition types implement these traits to get and
    /// set individual pieces of state.
    macro_rules! state_trait {
        ($doc:tt, $trait:ident, $caps:ty, $vp:ty, $save_state:ident, $package:expr,
            $(($field_num:literal, $id:expr, $get:ident, $set:ident, $ty:ty $(,)?)),* $(,)?
        ) => {
            #[doc = $doc]
            #[expect(clippy::allow_attributes)]
            pub trait $trait {
                type Error: 'static + std::error::Error + Send + Sync;

                /// Gets the partition's capabilities.
                fn caps(&self) -> &$caps;

                /// Commits any state changes made with the `set_*` methods.
                fn commit(&mut self) -> Result<(), Self::Error>;

                /// Checks that the state can be saved: that the elements that
                /// a save reads hold all of it. A backend refuses a state that
                /// they cannot hold, so that a save fails instead of saving
                /// a state that a restore would not reproduce.
                ///
                /// [`Self::save_all`] calls this before it reads any element,
                /// so the check must not change the state, and a refusal
                /// leaves the state unread. By default, every state can be
                /// saved.
                fn check_saveable(&mut self) -> Result<(), Self::Error> {
                    Ok(())
                }

                $(
                /// Gets the specified state.
                fn $get(&mut self) -> Result<$ty, Self::Error>;
                /// Sets the specified state.
                fn $set(&mut self, value: &$ty) -> Result<(), Self::Error>;
                )*

                /// Save all state that can be restored by a call to restore.
                #[allow(unused_mut)]
                fn save_all(&mut self) -> Result<$save_state, $crate::state::StateError<Self::Error>> {
                    self.check_saveable().map_err(|err| $crate::state::StateError{phase: "save check", err})?;
                    let mut save_state = $save_state::default();
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            save_state.$get = Some(self.$get().map_err(|err| $crate::state::StateError{phase: concat!("save ", stringify!($id)), err})?);
                        }
                    )*

                    Ok(save_state)
                }

                /// Restore state elements saved in save state.
                #[allow(unused_variables)]
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
                #[allow(unused_variables)]
                fn check_reset_all(&mut self, vp_info: &$vp) {
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::can_compare(self.caps()) && <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            assert_eq!(self.$get().expect($id), <$ty as $crate::state::StateElement<$caps, $vp>>::at_reset(self.caps(), vp_info), "reset state mismatch (actual/expected)");
                        }
                    )*
                }

                #[allow(unused_variables, unused_mut)]
                fn inspect_all(&mut self) -> impl ::inspect::InspectMut {
                    inspect::adhoc_mut(|req| {
                        let mut resp = req.respond();
                        $(
                            if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                                resp.field_with($id, || self.$get().ok());
                            }
                        )*
                    })
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

#[cfg(test)]
// The test state's trait has the methods that no test calls.
#[expect(dead_code)]
mod tests {
    use super::StateElement;
    use super::state_trait;
    use inspect::Inspect;
    use mesh_protobuf::Protobuf;
    use std::fmt::Error;

    pub struct Caps;
    pub struct Vp;

    #[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
    #[mesh(package = "virt.test")]
    pub struct Number {
        #[mesh(1)]
        value: u32,
    }

    impl StateElement<Caps, Vp> for Number {
        fn is_present(_caps: &Caps) -> bool {
            true
        }

        fn at_reset(_caps: &Caps, _vp: &Vp) -> Self {
            Self { value: 0 }
        }
    }

    state_trait! {
        "Test state",
        AccessTestState,
        Caps,
        Vp,
        TestSavedState,
        "virt.test",
        (1, "number", number, set_number, Number),
    }

    /// A state that every save accepts, which records the elements that a
    /// save reads.
    #[derive(Default)]
    struct Plain {
        reads: Vec<&'static str>,
    }

    impl AccessTestState for Plain {
        type Error = Error;

        fn caps(&self) -> &Caps {
            &Caps
        }

        fn commit(&mut self) -> Result<(), Error> {
            Ok(())
        }

        fn number(&mut self) -> Result<Number, Error> {
            self.reads.push("number");
            Ok(Number { value: 7 })
        }

        fn set_number(&mut self, _value: &Number) -> Result<(), Error> {
            Ok(())
        }
    }

    /// A state that refuses every save, which records its check.
    #[derive(Default)]
    struct Refusing(Plain);

    impl AccessTestState for Refusing {
        type Error = Error;

        fn caps(&self) -> &Caps {
            self.0.caps()
        }

        fn commit(&mut self) -> Result<(), Error> {
            self.0.commit()
        }

        fn check_saveable(&mut self) -> Result<(), Error> {
            self.0.reads.push("check_saveable");
            Err(Error)
        }

        fn number(&mut self) -> Result<Number, Error> {
            self.0.number()
        }

        fn set_number(&mut self, value: &Number) -> Result<(), Error> {
            self.0.set_number(value)
        }
    }

    #[test]
    fn a_save_accepts_a_state_by_default() {
        let mut state = Plain::default();
        let saved = state.save_all().unwrap();
        assert_eq!(
            saved,
            TestSavedState {
                number: Some(Number { value: 7 })
            }
        );
        assert_eq!(state.reads, ["number"]);
    }

    #[test]
    fn a_refused_save_reads_nothing() {
        let mut state = Refusing::default();
        let err = state.save_all().unwrap_err();
        assert_eq!(err.phase, "save check");
        assert!(std::error::Error::source(&err).is_some_and(|err| err.is::<Error>()));
        assert_eq!(state.0.reads, ["check_saveable"]);
    }
}
