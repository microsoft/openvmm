// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to define VM state.

pub(crate) use macros::state_trait;

use hvdef::HvRegisterValue;
use inspect::Inspect;
use std::fmt::Debug;
use thiserror::Error;

pub trait StateElement<C, V>: Sized + Inspect + PartialEq {
    /// Returns whether this state is present for the partition, based on the
    /// partition capabilities.
    ///
    /// A restore refuses a saved state that holds an element that is not
    /// present.
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

    /// Returns whether `restored`, which the partition reads back after a
    /// restore of `expected`, is `expected` up to what the hypervisor does not
    /// keep of it.
    ///
    /// A hypervisor can normalize a state that it is given: drop its reserved
    /// bits, or restore it in a form that reads back differently. A restore
    /// check that required equality would then reject a state that the
    /// hypervisor restored as well as it can hold it. An element that a
    /// hypervisor normalizes overrides this to accept the normalization that
    /// the hypervisor documents, and nothing else, so that a restore that
    /// loses more is still caught. By default, only an equal state is
    /// accepted.
    fn restore_eq(_caps: &C, restored: &Self, expected: &Self) -> bool {
        restored == expected
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

/// An error from saving, restoring, or resetting state.
#[derive(Debug, Error)]
pub enum StateError<T: 'static + Debug + std::error::Error> {
    /// The partition failed to access the state, in `phase`.
    #[error("state access error, phase {phase}")]
    Access {
        phase: &'static str,
        #[source]
        err: T,
    },
    /// A restore refused a saved state that holds `element`, which the
    /// partition does not have, before it changed any state.
    #[error(
        "state access error, phase restore check: the saved state has {element:?}, which the partition does not support"
    )]
    Unsupported { element: &'static str },
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
    /// The generated `restore_all` refuses a saved state that holds a piece
    /// that is missing for the partition, before it sets any piece, since a
    /// saved state is external input and its pieces are no proof of what the
    /// partition supports.
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
                    self.check_saveable().map_err(|err| $crate::state::StateError::Access{phase: "save check", err})?;
                    let mut save_state = $save_state::default();
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            save_state.$get = Some(self.$get().map_err(|err| $crate::state::StateError::Access{phase: concat!("save ", stringify!($id)), err})?);
                        }
                    )*

                    Ok(save_state)
                }

                /// Restore state elements saved in save state.
                ///
                /// A saved state can hold an element that the partition does
                /// not have, as a decoded snapshot of another partition can.
                /// This checks every element of the saved state first, and
                /// fails without setting or committing anything if the
                /// partition lacks one. It does not undo the elements that it
                /// has set when a setter fails.
                ///
                /// A debug build reads each element back after it sets it, and
                /// asserts that the state that it reads is the one that it set,
                /// up to the normalization that the element's `restore_eq`
                /// accepts.
                #[allow(unused_variables)]
                fn restore_all(&mut self, state: &$save_state) -> Result<(), $crate::state::StateError<Self::Error>> {
                    $(
                        if state.$get.is_some() && !<$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            return Err($crate::state::StateError::Unsupported{element: $id});
                        }
                    )*

                    $(
                        if let Some(value) = state.$get.as_ref() {
                            self.$set(value).map_err(|err| $crate::state::StateError::Access{phase: concat!("restore ", stringify!($id)), err})?;
                            if cfg!(debug_assertions) {
                                if <$ty as $crate::state::StateElement<$caps, $vp>>::can_compare(self.caps()) {
                                    let restored = self.$get().expect($id);
                                    assert!(
                                        <$ty as $crate::state::StateElement<$caps, $vp>>::restore_eq(self.caps(), &restored, value),
                                        "restore state mismatch (actual/expected): {:?} / {:?}",
                                        restored,
                                        value,
                                    );
                                }
                            }
                        }
                    )*

                    self.commit().map_err(|err| $crate::state::StateError::Access{phase: "commit restore", err})
                }

                /// Resets all the state elements to their initial state (after machine reset).
                fn reset_all(&mut self, vp_info: &$vp) -> Result<(), $crate::state::StateError<Self::Error>> {
                    $(
                        if <$ty as $crate::state::StateElement<$caps, $vp>>::is_present(self.caps()) {
                            self.$set(&<$ty as $crate::state::StateElement<$caps, $vp>>::at_reset(self.caps(), vp_info)).map_err(|err| $crate::state::StateError::Access{phase: concat!("reset ", stringify!($id)), err})?;
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
    use super::StateError;
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

    /// An element that a backend need not keep whole: of a restored value, only
    /// its low bit can read back differently.
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
    #[mesh(package = "virt.test")]
    pub struct Coarse {
        #[mesh(1)]
        value: u32,
    }

    impl StateElement<Caps, Vp> for Coarse {
        fn is_present(_caps: &Caps) -> bool {
            true
        }

        fn at_reset(_caps: &Caps, _vp: &Vp) -> Self {
            Self { value: 0 }
        }

        fn restore_eq(_caps: &Caps, restored: &Self, expected: &Self) -> bool {
            restored.value == expected.value & !1
        }
    }

    state_trait! {
        "Test state with an element that a backend can normalize",
        AccessCoarseState,
        Caps,
        Vp,
        CoarseSavedState,
        "virt.test.coarse",
        (1, "number", number, set_number, Number),
        (2, "coarse", coarse, set_coarse, Coarse),
    }

    /// A state that keeps of each restored value only the bits of a mask.
    struct Masked {
        number_mask: u32,
        coarse_mask: u32,
        number: u32,
        coarse: u32,
    }

    impl Masked {
        fn new(number_mask: u32, coarse_mask: u32) -> Self {
            Self {
                number_mask,
                coarse_mask,
                number: 0,
                coarse: 0,
            }
        }
    }

    impl AccessCoarseState for Masked {
        type Error = Error;

        fn caps(&self) -> &Caps {
            &Caps
        }

        fn commit(&mut self) -> Result<(), Error> {
            Ok(())
        }

        fn number(&mut self) -> Result<Number, Error> {
            Ok(Number { value: self.number })
        }

        fn set_number(&mut self, value: &Number) -> Result<(), Error> {
            self.number = value.value & self.number_mask;
            Ok(())
        }

        fn coarse(&mut self) -> Result<Coarse, Error> {
            Ok(Coarse { value: self.coarse })
        }

        fn set_coarse(&mut self, value: &Coarse) -> Result<(), Error> {
            self.coarse = value.value & self.coarse_mask;
            Ok(())
        }
    }

    fn restored(number: u32, coarse: u32) -> CoarseSavedState {
        CoarseSavedState {
            number: Some(Number { value: number }),
            coarse: Some(Coarse { value: coarse }),
        }
    }

    #[test]
    fn a_restore_accepts_the_normalization_that_an_element_documents() {
        let mut state = Masked::new(!0, !1);
        state.restore_all(&restored(5, 7)).unwrap();
        assert_eq!((state.number, state.coarse), (5, 6));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "restore state mismatch")]
    fn a_debug_restore_catches_more_than_an_element_documents() {
        // The backend also drops bit 1, which the element does not accept.
        Masked::new(!0, !3).restore_all(&restored(5, 7)).unwrap();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "restore state mismatch")]
    fn a_debug_restore_requires_an_equal_state_by_default() {
        // The number documents no normalization, so a dropped bit is a loss.
        Masked::new(!1, !1).restore_all(&restored(5, 7)).unwrap();
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
        assert!(matches!(
            err,
            StateError::Access {
                phase: "save check",
                ..
            }
        ));
        assert!(std::error::Error::source(&err).is_some_and(|err| err.is::<Error>()));
        assert_eq!(state.0.reads, ["check_saveable"]);
    }

    /// State with an element that only some partitions have, as the VP state
    /// has for an MSR that a CPUID feature enumerates.
    mod optional {
        use super::StateElement;
        use super::state_trait;
        use inspect::Inspect;
        use mesh_protobuf::Protobuf;
        use std::fmt::Error;

        pub struct Caps {
            has_extra: bool,
        }
        pub struct Vp;

        #[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
        #[mesh(package = "virt.test.optional")]
        pub struct Always {
            #[mesh(1)]
            value: u32,
        }

        impl StateElement<Caps, Vp> for Always {
            fn is_present(_caps: &Caps) -> bool {
                true
            }

            fn at_reset(_caps: &Caps, _vp: &Vp) -> Self {
                Self { value: 0 }
            }
        }

        #[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
        #[mesh(package = "virt.test.optional")]
        pub struct Extra {
            #[mesh(1)]
            value: u32,
        }

        impl StateElement<Caps, Vp> for Extra {
            fn is_present(caps: &Caps) -> bool {
                caps.has_extra
            }

            fn at_reset(_caps: &Caps, _vp: &Vp) -> Self {
                Self { value: 0 }
            }
        }

        state_trait! {
            "Test state with an optional element",
            AccessOptionalState,
            Caps,
            Vp,
            OptionalSavedState,
            "virt.test.optional",
            (1, "always", always, set_always, Always),
            (2, "extra", extra, set_extra, Extra),
        }

        /// A partition that holds its elements and records the ones that a
        /// restore sets, in order.
        struct Partition {
            caps: Caps,
            always: Always,
            extra: Extra,
            sets: Vec<&'static str>,
        }

        impl Partition {
            fn new(has_extra: bool) -> Self {
                Self {
                    caps: Caps { has_extra },
                    always: Always { value: 1 },
                    extra: Extra { value: 2 },
                    sets: Vec::new(),
                }
            }
        }

        impl AccessOptionalState for Partition {
            type Error = Error;

            fn caps(&self) -> &Caps {
                &self.caps
            }

            fn commit(&mut self) -> Result<(), Error> {
                Ok(())
            }

            fn always(&mut self) -> Result<Always, Error> {
                Ok(self.always)
            }

            fn set_always(&mut self, value: &Always) -> Result<(), Error> {
                self.sets.push("always");
                self.always = *value;
                Ok(())
            }

            fn extra(&mut self) -> Result<Extra, Error> {
                Ok(self.extra)
            }

            fn set_extra(&mut self, value: &Extra) -> Result<(), Error> {
                self.sets.push("extra");
                self.extra = *value;
                Ok(())
            }
        }

        #[test]
        fn a_save_holds_an_optional_element_only_where_the_partition_has_it() {
            assert_eq!(
                Partition::new(false).save_all().unwrap(),
                OptionalSavedState {
                    always: Some(Always { value: 1 }),
                    extra: None,
                }
            );
            assert_eq!(
                Partition::new(true).save_all().unwrap(),
                OptionalSavedState {
                    always: Some(Always { value: 1 }),
                    extra: Some(Extra { value: 2 }),
                }
            );
        }

        #[test]
        fn a_restore_leaves_an_optional_element_that_the_saved_state_lacks() {
            // A saved state from before the element existed, restored where
            // the partition has it, as after an upgrade.
            let mut partition = Partition::new(true);
            partition
                .restore_all(&OptionalSavedState {
                    always: Some(Always { value: 5 }),
                    extra: None,
                })
                .unwrap();
            assert_eq!(partition.sets, ["always"]);
            assert_eq!(partition.always, Always { value: 5 });
            assert_eq!(partition.extra, Extra { value: 2 });

            // A saved state with the element sets it.
            let mut partition = Partition::new(true);
            partition
                .restore_all(&OptionalSavedState {
                    always: Some(Always { value: 5 }),
                    extra: Some(Extra { value: 6 }),
                })
                .unwrap();
            assert_eq!(partition.sets, ["always", "extra"]);
            assert_eq!(partition.extra, Extra { value: 6 });
        }
    }

    /// An element that the test partition does not have.
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
    #[mesh(package = "virt.test")]
    pub struct Absent {
        #[mesh(1)]
        value: u32,
    }

    impl StateElement<Caps, Vp> for Absent {
        fn is_present(_caps: &Caps) -> bool {
            false
        }

        fn at_reset(_caps: &Caps, _vp: &Vp) -> Self {
            Self { value: 0 }
        }
    }

    state_trait! {
        "Test state with an element that the partition lacks",
        AccessRestoreState,
        Caps,
        Vp,
        RestoreSavedState,
        "virt.test",
        (1, "number", number, set_number, Number),
        (2, "absent", absent, set_absent, Absent),
    }

    /// A state that records each change that a restore makes.
    #[derive(Default)]
    struct Recording {
        number: u32,
        fail_set_number: bool,
        changes: Vec<&'static str>,
    }

    impl AccessRestoreState for Recording {
        type Error = Error;

        fn caps(&self) -> &Caps {
            &Caps
        }

        fn commit(&mut self) -> Result<(), Error> {
            self.changes.push("commit");
            Ok(())
        }

        fn number(&mut self) -> Result<Number, Error> {
            Ok(Number { value: self.number })
        }

        fn set_number(&mut self, value: &Number) -> Result<(), Error> {
            if self.fail_set_number {
                return Err(Error);
            }
            self.changes.push("set_number");
            self.number = value.value;
            Ok(())
        }

        fn absent(&mut self) -> Result<Absent, Error> {
            Err(Error)
        }

        fn set_absent(&mut self, _value: &Absent) -> Result<(), Error> {
            self.changes.push("set_absent");
            Ok(())
        }
    }

    #[test]
    fn a_restore_refuses_an_absent_element_before_it_changes_anything() {
        let mut state = Recording {
            number: 7,
            ..Default::default()
        };
        // The number comes first in the restore order, so a restore that
        // checked the elements one by one would have set it.
        let err = state
            .restore_all(&RestoreSavedState {
                number: Some(Number { value: 1 }),
                absent: Some(Absent { value: 2 }),
            })
            .unwrap_err();
        assert_eq!(state.changes, Vec::<&str>::new());
        assert_eq!(state.number, 7);
        assert!(matches!(err, StateError::Unsupported { element: "absent" }));
        let message = err.to_string();
        assert!(
            message.contains("restore check") && message.contains("absent"),
            "{message}"
        );
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn a_restore_sets_the_elements_that_the_partition_has() {
        let mut state = Recording::default();
        state
            .restore_all(&RestoreSavedState {
                number: Some(Number { value: 3 }),
                absent: None,
            })
            .unwrap();
        assert_eq!(state.changes, ["set_number", "commit"]);
        assert_eq!(state.number, 3);
    }

    #[test]
    fn a_failed_restore_keeps_the_error_of_the_backend() {
        let mut state = Recording {
            fail_set_number: true,
            ..Default::default()
        };
        let err = state
            .restore_all(&RestoreSavedState {
                number: Some(Number { value: 3 }),
                absent: None,
            })
            .unwrap_err();
        assert!(matches!(err, StateError::Access { .. }));
        assert!(std::error::Error::source(&err).is_some_and(|err| err.is::<Error>()));
        assert_eq!(state.changes, Vec::<&str>::new());
    }
}
