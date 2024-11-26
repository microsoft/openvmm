// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_std]

//! Provides the [`open_enum`] macro.

/// This macro creates an underlying struct that behaves like an enum, without
/// the undefined behavior of trying to match with a value outside of the enum.
/// The actual object created is a `#[repr(transparent)]` struct with a `pub` const
/// value. See comment in example below for an example of the underlying
/// structure.
///
/// This macro implements the following traits: `Copy`, `Clone`, `Debug`, `Eq`,
/// `PartialEq`, `Hash`, `Ord`, `PartialOrd`.
///
/// An example usage case for this macro is for protocols, when you want to use
/// an enum as a field in a struct that represents a specific type, like u16 or
/// u32. You are also able to convert to/from bytes with this typed enum.
///
/// # Examples
///
/// ```
/// # #[macro_use] extern crate open_enum; fn main() {
/// use open_enum::open_enum;
/// open_enum! {
///     #[expect(dead_code)] // This will apply to the generated struct defn
///     pub enum ExampleEnumName: u32 {
///         #![allow(missing_docs)] // This will apply to all subfields of the enum
///         THIS_IS_AN_ENUM = 32,
///     }
/// }
/// // Expands to:
/// //
/// // #[repr(transparent)]
//  // #[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
/// // #[expect(dead_code)]
/// // struct ExampleEnumName(u32);
/// //
/// // #[expect(missing_docs)]
/// // impl ExampleEnumName {
/// //     pub const THIS_IS_AN_ENUM: ExampleEnumName = ExampleEnumName(0)
/// // }
///
/// // To access an element in ExampleEnumName
/// let example_enum = ExampleEnumName::THIS_IS_AN_ENUM;
///
/// assert_eq!(example_enum.0, 32);
///
/// let number: u32 = example_enum.0; // enum value is type u32
/// # }
/// ```
#[macro_export]
macro_rules! open_enum {
    (
        $(#[$a:meta])*
        $v:vis enum $name:ident : $storage:ty {
            $(#![$implattr:meta])*
            $(
                $(#[$vattr:meta])*
                $variant:ident = $value:expr,
            )*
        }
    ) => {
        #[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
        #[repr(transparent)]
        $(#[$a])*
        $v struct $name(pub $storage);
        $(#[$implattr])*
        impl $name {
            $(
                $(#[$vattr])*
                pub const $variant: $name = $name($value);
            )*
        }
        impl ::core::fmt::Debug for $name {
            fn fmt(&self, fmt: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
                #![allow(unreachable_patterns)]
                let s = match *self {
                    $( Self::$variant => stringify!($variant), )*
                    _ => {
                        return ::core::fmt::Debug::fmt(&self.0, fmt);
                    }
                };
                fmt.pad(s)
            }
        }
    }
}
