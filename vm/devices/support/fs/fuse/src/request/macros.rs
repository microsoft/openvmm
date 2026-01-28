// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Generates an enum that holds fuse operations and their arguments.
macro_rules! fuse_operations {
    ($( $opcode:ident $name:ident $($arg_name:ident : $arg_type:tt)* ; )*) => {
        /// Represents a FUSE message and its arguments.
        pub enum FuseOperation {
            /// An operation where the header could be parsed, but the remainder of the message
            /// could not.
            Invalid,
            /// An operation where parsing failed with a specific error that should be returned
            /// to the client.
            Error(lx::Error),
            $(
                $name {
                    $($arg_name: fuse_operations!(@to_type $arg_type),)*
                },
            )*
        }

        impl FuseOperation {
            /// Create a FuseOperation for the specified opcode, reading the arguments from the
            /// reader.
            pub fn read(opcode: u32, mut reader: impl RequestReader) -> lx::Result<Self> {
                let op = match opcode {
                    $($opcode => {
                        $(
                            let $arg_name: fuse_operations!(@to_type $arg_type) = fuse_operations!(@to_read reader $arg_type);
                        )*
                        Self::$name {
                            $(
                                $arg_name,
                            )*
                        }
                    },)*
                    _ => {
                        tracing::error!(opcode, "Invalid opcode");
                        return Err(lx::Error::EINVAL)
                    }
                };

                Ok(op)
            }
        }

        impl std::fmt::Debug for FuseOperation {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    FuseOperation::Invalid => f.write_str("Invalid"),
                    FuseOperation::Error(e) => f.debug_tuple("Error").field(e).finish(),
                    $(
                        FuseOperation::$name { $($arg_name,)* } => {
                            let mut d = f.debug_struct(stringify!($name));
                            $(
                                fuse_operations!(@to_debug d $arg_name $arg_type);
                            )*
                            d.finish()
                        }
                    )*
                }
            }
        }
    };

    // Convert type name to the field type.
    (@to_type name) => { lx::LxString };
    (@to_type str) => { lx::LxString };
    (@to_type [u8; $arg:ident.$field:ident]) => { Box<[u8]> };
    (@to_type [u8]) => { Box<[u8]> };
    (@to_type $t:tt) => { $t };

    // Convert type name to the associated reader method.
    (@to_read $name:ident name) => { $name.name()? };
    (@to_read $name:ident str) => { $name.string()? };
    (@to_read $name:ident [u8]) => { $name.read_all()? };
    (@to_read $name:ident [u8; $arg:ident.$field:ident]) => { $name.read_count($arg.$field as usize)? };
    (@to_read $name:ident $t:tt) => { $name.read_type()? };

    (@to_debug $debug:ident $name:ident [u8]) => { $debug.field(stringify!($name), &$name.len()) };
    (@to_debug $debug:ident $name:ident [u8; $arg:ident.$field:ident]) => { $debug.field(stringify!($name), &$name.len()) };
    (@to_debug $debug:ident $name:ident $t:tt) => { $debug.field(stringify!($name), $name); };
}
