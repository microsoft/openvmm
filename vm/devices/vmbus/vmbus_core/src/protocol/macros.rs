// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Defines an open enum, which contains constants for each vmbus message type.
macro_rules! vmbus_message_type {
    (pub enum $enum_name:ident, $open_enum_name:ident { $( $num:literal $name:ident $rest:tt, )* }) => {
        open_enum! {
            /// Represents the message type value that identifies a vmbus protocol message.
            #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
            pub enum $open_enum_name: u32 {
                $($name = $num,)*
            }
        }
    }
}

/// Defines an enum which contains a variant for each message type, and a parse method that converts
/// a received message into one of those variants.
macro_rules! vmbus_message_enum {
    (pub enum $enum_name:ident, $open_enum_name:ident { $( $num:literal $name:ident { $($type:ident $min_version:tt $($condition_name:ident:$condition_value:tt)*),* } ,)* }) => {
        /// Represents a parsed vmbus protocol message.
        #[derive(Debug)]
        pub enum $enum_name<'a> {
            $( $($type($type, &'a [u8]),)* )*
        }

        impl<'a> $enum_name<'a> {
            /// Parses a vmbus message received from the synic into an enum variant, only
            /// parsing messages that are supported by the specified protocol version.
            ///
            /// Use `None` for the version to only parse messages that are accepted in a
            /// disconnected state.
            pub fn parse(data: &'a [u8], version: Option<VersionInfo>) -> Result<Self, ParseError> {
                let (version, features) = if let Some(version) = version {
                    (Some(version.version), version.feature_flags)
                } else {
                    (None, FeatureFlags::new())
                };

                // TODO: zerocopy: use Result returned by `read_from_prefix` in the returned `MessageTooSmall` error. (https://github.com/microsoft/openvmm/issues/759)
                let (header, data) = MessageHeader::read_from_prefix(data).map_err(|_| ParseError::MessageTooSmall(None))?;

                let message = match header.message_type {
                    $(
                        $($open_enum_name::$name
                            if vmbus_message_enum!(@create_conditions $type version features data $min_version $($condition_name:$condition_value)*) =>
                        {
                            // TODO: zerocopy: use Result returned by `read_from_prefix` in the returned `MessageTooSmall` error. (https://github.com/microsoft/openvmm/issues/759)
                            let (message, remaining) = $type::read_from_prefix(data).map_err(|_| ParseError::MessageTooSmall(Some(header.message_type)))?;

                            Self::$type(message, remaining)
                        })*
                    )*
                    _ => return Err(ParseError::InvalidMessageType(header.message_type)),
                };

                Ok(message)
            }
        }
    };

    (@create_conditions $type:ident $version_ident:ident $features_ident:ident $data_ident:ident $min_version:tt $($name:ident:$value:tt)*) => {
        $version_ident >= vmbus_message_enum!(@to_version $min_version)
        $(&& vmbus_message_enum!(@create_condition $type $features_ident $data_ident $name $value))*
    };

    (@create_condition $type:ident $features_ident:ident $data_ident:ident features $min_features:tt) => {
       vmbus_message_enum!(@to_features $features_ident $min_features)
    };

    (@create_condition  $type:ident $features_ident:ident $data_ident:ident check_size true) => {
        $data_ident.len() >= size_of::<$type>()
    };

    (@to_version 0) => { None };
    (@to_version $version:ident) => { Some(Version::$version) };

    (@to_features $features_ident:ident $flag:ident) => { $features_ident.$flag() };
    (@to_features $features_ident:ident ($flag1:ident | $flag2:ident)) => { ($features_ident.$flag1() || $features_ident.$flag2()) };
}

/// Implements the `VmbusMessage` trait for each protocol message struct, which provides a constant
/// with the message type for that struct. It also generates a compile-time assert that the message
/// fits in the hypervisor message payload.
macro_rules! vmbus_message_trait_impl {
    (pub enum $enum_name:ident, $open_enum_name:ident { $( $num:literal $name:ident { $($type:ident $min_version:tt $($condition_name:ident:$condition_value:tt)*),* } ,)* }) => {
        $($(
            impl VmbusMessage for $type {
                const MESSAGE_TYPE: $open_enum_name = $open_enum_name::$name;
            }

            static_assertions::const_assert!($type::MESSAGE_SIZE <= MAX_MESSAGE_SIZE);
        )*)*
    }
}

/// Defines an open enum with message type constant, an enum with parsed messages, and
/// `VmbusMessage` trait implementation for vmbus protocol messages. See this macro's usage in
/// protocol.rs for more information.
macro_rules! vmbus_messages {
    ($($contents:tt)*) => {
        vmbus_message_type!($($contents)*);
        vmbus_message_enum!($($contents)*);
        vmbus_message_trait_impl!($($contents)*);
    }
}
