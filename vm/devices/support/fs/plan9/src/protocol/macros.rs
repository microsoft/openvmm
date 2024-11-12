// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Defines a struct to hold a 9p message's fields.
macro_rules! p9_message_struct {
    ($( $num:literal $name:ident $($field_name:ident [$field_type:tt] )* ;)*) => {
        $(
            // The struct holds the reader it was created from for two reasons:
            // 1. Some operations (e.g. Twrite) need it to access additional data.
            // 2. It allows the lifetime 'a to be there unconditionally; otherwise, only some
            //    messages would need it and the macro can't easily filter on that.
            #[allow(dead_code)]
            pub struct $name<'a> {
                pub reader: super::SliceReader<'a>,
                $(pub $field_name: p9_message_struct!(@to_type $field_type),)*
            }

            // Create a message from a slice reader.
            impl<'a> TryFrom<super::SliceReader<'a>> for $name<'a> {
                type Error = lx::Error;

                p9_message_struct!(@try_from $name $($field_name [$field_type])*);
            }

            // Custom Debug trait because the reader field must be excluded.
            impl<'a> std::fmt::Debug for $name<'a> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.debug_struct(stringify!($name))
                    $(
                        .field(stringify!($field_name), &self.$field_name)
                    )*
                        .finish()
                }
            }
        )*
    };

    // Convert size to the field type.
    (@to_type s) => { &'a lx::LxStr };
    (@to_type n) => { &'a lx::LxStr };
    (@to_type q) => { Qid };
    (@to_type ns) => { super::NameIterator<'a> };
    (@to_type qs) => { super::QidIterator<'a> };
    (@to_type 2) => { u16 };
    (@to_type 4) => { u32 };
    (@to_type 8) => { u64 };

    // Convert size to the associated reader method.
    (@to_read $name:ident s) => { $name.string()? };
    (@to_read $name:ident n) => { $name.name()? };
    (@to_read $name:ident q) => { $name.qid()? };
    (@to_read $name:ident ns) => { $name.names()? };
    (@to_read $name:ident qs) => { $name.qids()? };
    (@to_read $name:ident 2) => { $name.u16()? };
    (@to_read $name:ident 4) => { $name.u32()? };
    (@to_read $name:ident 8) => { $name.u64()? };

    // Generate the try_from method for a message with fields.
    (@try_from $name:ident $($field_name:ident [$field_type:tt] )+) => {
        fn try_from(mut reader: super::SliceReader<'a>) -> lx::Result<$name<'a>> {
            $(
                let $field_name = p9_message_struct!(@to_read reader $field_type);
            )+
            Ok($name {
                reader,
                $(
                    $field_name,
                )+
            })
        }
    };

    // The case of a message with no fields must be handled separately so the compiler doesn't
    // complain about an unnecessary "mut" on the argument.
    (@try_from $name:ident) => {
        fn try_from(reader: super::SliceReader<'a>) -> lx::Result<$name<'a>> {
            Ok($name {
                reader,
            })
        }
    };
}

// Generate the Plan9Message enum.
macro_rules! p9_message_enum {
    ($( $num:literal $name:ident $($field_name:ident [$field_type:tt] )* ;)*) => {
        #[allow(dead_code)]
        #[derive(Debug)]
        pub enum Plan9Message<'a> {
            $($name($name<'a>),)*
        }

        impl<'a> Plan9Message<'a> {
            // Create a Plan9Message for the specified message type, reading the fields from the
            // reader.
            pub fn read(message_type: u8, reader: super::SliceReader<'a>) -> lx::Result<Plan9Message<'a>> {
                let message = match message_type {
                    $($num => Plan9Message::$name(reader.try_into()?),)*
                    _ => {
                        tracing::warn!(message_type, "[9P] Unhandled message type");
                        return Err(lx::Error::EINVAL)
                    }
                };

                Ok(message)
            }
        }
    };
}

// Generate structs and an enum to represent 9p protocol messages.
#[macro_export]
macro_rules! p9_protocol_messages {
    ($($contents:tt)*) => {
        p9_message_struct!($($contents)*);
        p9_message_enum!($($contents)*);
    }
}
