// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

macro_rules! lx_errors {
    ($($name:ident = $value:expr;)*) => {
        $(pub const $name: i32 = $value;)*

        /// Returns a string representation of the error.
        ///
        /// Unlike libc's strerror, this just returns the string version of the constant, e.g
        /// "ENOENT".
        pub fn str_error(err: i32) -> &'static str {
            match err {
                $($value => stringify!($name),)*
                _ => "Unknown error"
            }
        }

        impl Error {
            $(pub const $name: Self = Self($name);)*
        }
    };
}
