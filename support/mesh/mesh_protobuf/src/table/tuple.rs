// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Table definitions for tuples.

use super::StructMetadata;
use super::TableEncoder;
use super::decode::ErasedDecoderEntry;
use super::decode::StructDecodeMetadata;
use super::encode::ErasedEncoderEntry;
use super::encode::StructEncodeMetadata;
use crate::DefaultEncoding;
use crate::FieldDecode;
use crate::FieldEncode;
use crate::protofile::DescribeField;
use crate::protofile::DescribeMessage;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;

impl DescribeMessage<()> for TableEncoder {
    const DESCRIPTION: MessageDescription<'static> = MessageDescription::External {
        name: "google.protobuf.Empty",
        import_path: "google/protobuf/empty.proto",
    };
}

macro_rules! tuplegen {
    { $(
        ($count:expr, $( ($t:ident, $e:ident, $u:ident, $n:tt) ),*  $(,)?)
        ),* $(,)?
    } => {
        $(
        // SAFETY: macro caller ensures all fields are described.
        unsafe impl<'a, $($t,)*> StructMetadata for ($($t,)*) {
            const NUMBERS: &'static [u32] = &[$($n + 1,)*];
            const OFFSETS: &'static [usize] = &[$(core::mem::offset_of!(Self, $n),)*];
        }

        // SAFETY: macro caller ensures all fields are described.
        unsafe impl<R, $($t,)*> StructEncodeMetadata<R> for ($($t,)*)
        where
            $($t: DefaultEncoding,)*
            $($t::Encoding: FieldEncode<$t, R>,)*
        {
            const ENCODERS: &'static [ErasedEncoderEntry] = &[$(<$t::Encoding>::ENTRY.erase(),)*];
        }

        // SAFETY: macro caller ensures all fields are described.
        unsafe impl<'de, R, $($t,)*> StructDecodeMetadata<'de, R> for ($($t,)*)
        where
            $($t: DefaultEncoding,)*
            $($t::Encoding: FieldDecode<'de, $t, R>,)*
        {
            const DECODERS: &'static [ErasedDecoderEntry] = &[$(<$t::Encoding>::ENTRY.erase(),)*];
        }

        impl<$($t: DefaultEncoding,)*> DefaultEncoding for ($($t,)*) {
            type Encoding = TableEncoder;
        }

        impl<$($t,)*> DescribeField<($($t,)*)> for TableEncoder
        where
            $($t: DefaultEncoding,)*
            $($t::Encoding: DescribeField<$t>,)*
        {
            const FIELD_TYPE: FieldType<'static> = FieldType::tuple(&[$(<$t::Encoding as DescribeField<$t>>::FIELD_TYPE,)*]);
        }
        )*
    };
}

tuplegen! {
    (0, ),
    (1, (E0, T0, U0, 0)),
    (2, (E0, T0, U0, 0), (E1, T1, U1, 1)),
    (3, (E0, T0, U0, 0), (E1, T1, U1, 1), (E2, T2, U2, 2)),
    (4, (E0, T0, U0, 0), (E1, T1, U1, 1), (E2, T2, U2, 2), (E3, T3, U3, 3)),
    (5, (E0, T0, U0, 0), (E1, T1, U1, 1), (E2, T2, U2, 2), (E3, T3, U3, 3), (E4, T4, U4, 4)),
    (6, (E0, T0, U0, 0), (E1, T1, U1, 1), (E2, T2, U2, 2), (E3, T3, U3, 3), (E4, T4, U4, 4), (E5, T5, U5, 5)),
    (7, (E0, T0, U0, 0), (E1, T1, U1, 1), (E2, T2, U2, 2), (E3, T3, U3, 3), (E4, T4, U4, 4), (E5, T5, U5, 5), (E6, T6, U6, 6)),
}
