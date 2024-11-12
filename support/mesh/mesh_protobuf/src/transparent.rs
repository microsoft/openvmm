// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transparent encoding for types that are a wrapper around another type.

use crate::inplace::InplaceOption;
use crate::protobuf::FieldReader;
use crate::protobuf::FieldSizer;
use crate::protobuf::FieldWriter;
use crate::protobuf::MessageReader;
use crate::protobuf::MessageSizer;
use crate::protobuf::MessageWriter;
use crate::protofile::DescribeField;
use crate::protofile::DescribeMessage;
use crate::protofile::FieldType;
use crate::protofile::MessageDescription;
use crate::table::decode::DecoderEntry;
use crate::table::encode::EncoderEntry;
use crate::FieldDecode;
use crate::FieldEncode;
use crate::MessageDecode;
use crate::MessageEncode;
use crate::Result;
use core::mem::MaybeUninit;
use core::ptr;

/// A type that can be encoded as a transparent wrapper around an inner type.
///
/// # Safety
/// The caller must ensure that there is only one non-zero-sized field in the
/// struct, at the provided offset, and that constructing the struct by writing
/// just that field is safe.
pub unsafe trait Transparent {
    /// The inner type.
    type Inner;
    /// The offset of the inner type. This should almost always be zero unless
    /// Rust decides to add some padding at the beginning in some debug mode.
    const OFFSET: usize;
}

/// An encoding derived by `mesh_derive` for a transparent type, using inner
/// encoding `E`.
#[derive(Copy, Clone)]
pub struct TransparentEncoding<E>(E);

impl<T: Transparent, E: DescribeField<T::Inner>> DescribeField<T> for TransparentEncoding<E> {
    const FIELD_TYPE: FieldType<'static> = E::FIELD_TYPE;
    const PACKED_TYPE: Option<&'static str> = E::PACKED_TYPE;
}

impl<T: Transparent, E: DescribeMessage<T::Inner>> DescribeMessage<T> for TransparentEncoding<E> {
    const DESCRIPTION: MessageDescription<'static> = E::DESCRIPTION;
}

impl<T: Transparent, E: MessageEncode<T::Inner, R>, R> MessageEncode<T, R>
    for TransparentEncoding<E>
{
    fn write_message(item: T, writer: MessageWriter<'_, '_, R>) {
        let item = MaybeUninit::new(item);
        // SAFETY: by the `Transparent` trait, there is a valid field with the
        // inner type at the specified offset.
        let inner = unsafe { item.as_ptr().byte_add(T::OFFSET).cast::<T::Inner>().read() };
        E::write_message(inner, writer)
    }

    fn compute_message_size(item: &mut T, sizer: MessageSizer<'_>) {
        // SAFETY: by the `Transparent` trait, there is a valid field with the
        // inner type at the specified offset.
        let inner = unsafe { &mut *ptr::from_mut(item).byte_add(T::OFFSET).cast::<T::Inner>() };
        E::compute_message_size(inner, sizer)
    }
}

impl<'de, T: Transparent, E: MessageDecode<'de, T::Inner, R>, R> MessageDecode<'de, T, R>
    for TransparentEncoding<E>
{
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: MessageReader<'de, '_, R>,
    ) -> Result<()> {
        let init = item.forget();
        // SAFETY: by the `Transparent` trait, the inner type is valid memory
        // for write at the specified offset.
        let inner = unsafe {
            &mut *item
                .as_mut_ptr()
                .byte_add(T::OFFSET)
                .cast::<MaybeUninit<T::Inner>>()
        };
        let mut inner = if init {
            // SAFETY: the outer value is initialized, so the inner one is, too.
            unsafe { InplaceOption::new_init_unchecked(inner) }
        } else {
            InplaceOption::uninit(inner)
        };
        let r = E::read_message(&mut inner, reader);
        if inner.forget() {
            // SAFETY: the inner value is initialized, so by the `Transparent`
            // trait, the outer one is, too.
            unsafe { item.set_init_unchecked() };
        }
        r
    }
}

impl<T: Transparent, E: FieldEncode<T::Inner, R>, R> FieldEncode<T, R> for TransparentEncoding<E> {
    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        let item = MaybeUninit::new(item);
        // SAFETY: by the `Transparent` trait, there is a valid field with the
        // inner type at the specified offset.
        let inner = unsafe { item.as_ptr().byte_add(T::OFFSET).cast::<T::Inner>().read() };
        E::write_field(inner, writer)
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        // SAFETY: by the `Transparent` trait, there is a valid field with the
        // inner type at the specified offset.
        let inner = unsafe { &mut *ptr::from_mut(item).byte_add(T::OFFSET).cast::<T::Inner>() };
        E::compute_field_size(inner, sizer)
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }

    const ENTRY: EncoderEntry<T, R> = {
        // If there is no leading padding, then just use the inner entry
        // directly.
        if T::OFFSET == 0 {
            // SAFETY: by the `Transparent` trait, there is a valid field with
            // the inner type, and we know it to be offset zero. So we can
            // encode this type by encoding it as if it is the inner type.
            unsafe { EncoderEntry::new_unchecked(E::ENTRY.erase()) }
        } else {
            // We could probably use a table entry here, but in practice this
            // path won't hit so don't bother.
            EncoderEntry::custom::<Self>()
        }
    };
}

impl<'de, T: Transparent, E: FieldDecode<'de, T::Inner, R>, R> FieldDecode<'de, T, R>
    for TransparentEncoding<E>
{
    fn read_field(item: &mut InplaceOption<'_, T>, reader: FieldReader<'de, '_, R>) -> Result<()> {
        let init = item.forget();
        // SAFETY: by the `Transparent` trait, the inner type is valid memory
        // for write at the specified offset.
        let inner = unsafe {
            &mut *item
                .as_mut_ptr()
                .byte_add(T::OFFSET)
                .cast::<MaybeUninit<T::Inner>>()
        };
        let mut inner = if init {
            // SAFETY: the outer value is initialized, so the inner one is, too.
            unsafe { InplaceOption::new_init_unchecked(inner) }
        } else {
            InplaceOption::uninit(inner)
        };
        let r = E::read_field(&mut inner, reader);
        if inner.forget() {
            // SAFETY: the inner value is initialized, so by the `Transparent`
            // trait, the outer one is, too.
            unsafe { item.set_init_unchecked() };
        }
        r
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> Result<()> {
        let init = item.forget();
        // SAFETY: by the `Transparent` trait, the inner type is valid memory
        // for write at the specified offset.
        let inner = unsafe {
            &mut *item
                .as_mut_ptr()
                .byte_add(T::OFFSET)
                .cast::<MaybeUninit<T::Inner>>()
        };
        let mut inner = if init {
            // SAFETY: the outer value is initialized, so the inner one is, too.
            unsafe { InplaceOption::new_init_unchecked(inner) }
        } else {
            InplaceOption::uninit(inner)
        };
        let r = E::default_field(&mut inner);
        if inner.forget() {
            // SAFETY: the inner value is initialized, so by the `Transparent`
            // trait, the outer one is, too.
            unsafe { item.set_init_unchecked() };
        }
        r
    }

    fn wrap_in_sequence() -> bool {
        E::wrap_in_sequence()
    }

    const ENTRY: DecoderEntry<'de, T, R> = {
        // If there is no leading padding, then just use the inner entry
        // directly.
        if T::OFFSET == 0 {
            // SAFETY: by the `Transparent` trait, the outer type can be
            // initialized by initializing the inner type, and we know it to be
            // at offset zero. So we can decode this type by decoding it as if
            // it is the inner type.
            unsafe { DecoderEntry::new_unchecked(E::ENTRY.erase()) }
        } else {
            // We could probably use a table entry here, but in practice this
            // path won't hit so don't bother.
            DecoderEntry::custom::<Self>()
        }
    };
}
