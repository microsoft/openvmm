// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Table-based encoding.

use super::StructMetadata;
use super::TableEncoder;
use crate::protobuf::FieldSizer;
use crate::protobuf::FieldWriter;
use crate::protobuf::MessageSizer;
use crate::protobuf::MessageWriter;
use crate::FieldEncode;
use crate::MessageEncode;
use alloc::slice;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

impl<T, R> MessageEncode<T, R> for TableEncoder
where
    T: StructEncodeMetadata<R>,
{
    fn write_message(item: T, writer: MessageWriter<'_, '_, R>) {
        let mut item = MaybeUninit::new(item);
        // SAFETY: `T` guarantees that its encoders and offsets are correct for
        // this type.
        unsafe {
            write_fields(
                T::NUMBERS,
                T::ENCODERS,
                T::OFFSETS,
                item.as_mut_ptr().cast(),
                writer,
            );
        }
    }

    fn compute_message_size(item: &mut T, sizer: MessageSizer<'_>) {
        // SAFETY: `T` guarantees that its encoders and offsets are correct for
        // this type.
        unsafe {
            compute_size_fields::<R>(
                T::NUMBERS,
                T::ENCODERS,
                T::OFFSETS,
                core::ptr::from_mut(item).cast::<u8>(),
                sizer,
            );
        }
    }
}

impl<T, R> FieldEncode<T, R> for TableEncoder
where
    T: StructEncodeMetadata<R>,
{
    // Override the default implementation to use the table encoder directly.
    // This saves code size by avoiding extra stub functions and vtables.
    const ENTRY: EncoderEntry<T, R> = EncoderEntry::table();

    fn write_field(item: T, writer: FieldWriter<'_, '_, R>) {
        let mut item = MaybeUninit::new(item);
        // SAFETY: `T` guarantees that its encoders and offsets are correct for
        // this type.
        unsafe {
            write_message(
                T::NUMBERS,
                T::ENCODERS,
                T::OFFSETS,
                item.as_mut_ptr().cast(),
                writer,
            );
        }
    }

    fn compute_field_size(item: &mut T, sizer: FieldSizer<'_>) {
        // SAFETY: `T` guarantees that its encoders and offsets are correct for
        // this type.
        unsafe {
            compute_size_message::<R>(
                T::NUMBERS,
                T::ENCODERS,
                T::OFFSETS,
                core::ptr::from_mut(item).cast::<u8>(),
                sizer,
            );
        }
    }
}

unsafe fn write_message<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    writer: FieldWriter<'_, '_, R>,
) {
    assert_eq!(numbers.len(), encoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by the caller.
    unsafe {
        write_message_by_ptr(
            numbers.len(),
            numbers.as_ptr(),
            encoders.as_ptr(),
            offsets.as_ptr(),
            base,
            writer,
        )
    }
}

#[inline(never)]
unsafe fn write_message_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    encoders: *const ErasedEncoderEntry,
    offsets: *const usize,
    base: *mut u8,
    writer: FieldWriter<'_, '_, R>,
) {
    // SAFETY: guaranteed by the caller.
    writer.message(|writer| unsafe {
        write_fields_inline(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(encoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            writer,
        )
    })
}

/// Writes the fields of a message using the provided metadata.
///
/// Note that `base` will no longer contain a valid object after this function
/// returns.
///
/// # Safety
/// The caller must ensure that the provided encoders and offsets correspond to
/// fields in the struct at `base`, and that `base` is owned.
#[doc(hidden)] // only used publicly for `mesh_derive`
pub unsafe fn write_fields<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    writer: MessageWriter<'_, '_, R>,
) {
    assert_eq!(numbers.len(), encoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by the caller.
    unsafe {
        write_fields_by_ptr(
            numbers.len(),
            numbers.as_ptr(),
            encoders.as_ptr(),
            offsets.as_ptr(),
            base,
            writer,
        )
    }
}

#[inline(never)]
unsafe fn write_fields_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    encoders: *const ErasedEncoderEntry,
    offsets: *const usize,
    base: *mut u8,
    writer: MessageWriter<'_, '_, R>,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        write_fields_inline(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(encoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            writer,
        )
    }
}

unsafe fn write_fields_inline<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    mut writer: MessageWriter<'_, '_, R>,
) {
    for ((&number, encoder), &offset) in numbers.iter().zip(encoders).zip(offsets) {
        let writer = writer.field(number);
        // SAFETY: the caller guarantees that `base` points to an object
        // compatible with this encoder and that it will not access the object
        // through `base` after this returns.
        unsafe {
            let ptr = base.add(offset);
            encoder.write_field(ptr, writer);
        }
    }
}

unsafe fn compute_size_message<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    sizer: FieldSizer<'_>,
) {
    assert_eq!(numbers.len(), encoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by the caller.
    unsafe {
        compute_size_message_by_ptr::<R>(
            numbers.len(),
            numbers.as_ptr(),
            encoders.as_ptr(),
            offsets.as_ptr(),
            base,
            sizer,
        )
    }
}

#[inline(never)]
unsafe fn compute_size_message_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    encoders: *const ErasedEncoderEntry,
    offsets: *const usize,
    base: *mut u8,
    sizer: FieldSizer<'_>,
) {
    // SAFETY: guaranteed by the caller.
    sizer.message(|sizer| unsafe {
        compute_size_fields_inline::<R>(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(encoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            sizer,
        )
    })
}

/// Computes the size of a message using the provided metadata.
///
/// # Safety
/// The caller must ensure that the provided encoders and offsets correspond to
/// fields in the struct at `base`, and that `base` is valid for write.
#[doc(hidden)] // only used publicly for `mesh_derive`
pub unsafe fn compute_size_fields<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    sizer: MessageSizer<'_>,
) {
    assert_eq!(numbers.len(), encoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by the caller.
    unsafe {
        compute_size_fields_by_ptr::<R>(
            numbers.len(),
            numbers.as_ptr(),
            encoders.as_ptr(),
            offsets.as_ptr(),
            base,
            sizer,
        )
    }
}

#[inline(never)]
unsafe fn compute_size_fields_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    encoders: *const ErasedEncoderEntry,
    offsets: *const usize,
    base: *mut u8,
    sizer: MessageSizer<'_>,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        compute_size_fields_inline::<R>(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(encoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            sizer,
        )
    }
}

unsafe fn compute_size_fields_inline<R>(
    numbers: &[u32],
    encoders: &[ErasedEncoderEntry],
    offsets: &[usize],
    base: *mut u8,
    mut sizer: MessageSizer<'_>,
) {
    for ((&number, encoder), &offset) in numbers.iter().zip(encoders).zip(offsets) {
        let sizer = sizer.field(number);
        // SAFETY: the caller guarantees that `base` points to an object
        // compatible with this encoder.
        unsafe {
            let ptr = base.add(offset);
            encoder.compute_size_field::<R>(ptr, sizer);
        }
    }
}

/// Metadata for encoding a struct.
///
/// # Safety
///
/// The implementor must ensure that the `ENCODERS` are correct and complete for
/// `Self` and `R`.
pub unsafe trait StructEncodeMetadata<R>: StructMetadata {
    /// The list of encoder vtables.
    const ENCODERS: &'static [ErasedEncoderEntry];
}

/// An entry in the encoder table.
///
/// This contains the metadata necessary to apply an encoder to a field.
///
/// This cannot be instantiated directly; use [`FieldEncode::ENTRY`] to get an
/// instance for a particular encoder.
pub struct EncoderEntry<T, R>(ErasedEncoderEntry, PhantomData<fn(T, &mut R)>);

impl<T, R> EncoderEntry<T, R> {
    /// # Safety
    /// The caller must ensure that the erased entry is an valid entry for `T`
    /// and `R`.
    pub(crate) const unsafe fn new_unchecked(entry: ErasedEncoderEntry) -> Self {
        Self(entry, PhantomData)
    }

    /// Returns an encoder entry that contains a vtable with methods for
    /// encoding the field.
    pub(crate) const fn custom<E: FieldEncode<T, R>>() -> Self {
        Self(
            ErasedEncoderEntry(
                core::ptr::from_ref(
                    const {
                        &StaticEncoderVtable {
                            write_fn: write_field_dyn::<T, R, E>,
                            compute_size_fn: compute_size_field_dyn::<T, R, E>,
                        }
                    },
                )
                .cast::<()>(),
            ),
            PhantomData,
        )
    }

    /// Returns an encoder entry that contains an encoder table.
    const fn table() -> Self
    where
        T: StructEncodeMetadata<R>,
    {
        Self(
            ErasedEncoderEntry(
                core::ptr::from_ref(
                    const {
                        &EncoderTable {
                            count: T::NUMBERS.len(),
                            numbers: T::NUMBERS.as_ptr(),
                            encoders: T::ENCODERS.as_ptr(),
                            offsets: T::OFFSETS.as_ptr(),
                        }
                    },
                )
                .cast::<()>()
                .wrapping_byte_add(ENTRY_IS_TABLE),
            ),
            PhantomData,
        )
    }

    /// Return the type-erased encoder entry.
    pub const fn erase(&self) -> ErasedEncoderEntry {
        self.0
    }
}

/// An type-erased version of [`EncoderEntry`], for use in a
/// [`StructEncodeMetadata::ENCODERS`] table.
//
// Internally, this is a pointer to either a vtable or a table.
// The low bit is used to distinguish between the two.
#[derive(Copy, Clone, Debug)]
pub struct ErasedEncoderEntry(*const ());

// SAFETY: the entry represents a set of integers and function pointers, which
// have no cross-thread constraints.
unsafe impl Send for ErasedEncoderEntry {}
// SAFETY: the entry represents a set of integers and function pointers, which
// have no cross-thread constraints.
unsafe impl Sync for ErasedEncoderEntry {}

const ENTRY_IS_TABLE: usize = 1;

const _: () = assert!(align_of::<ErasedEncoderEntry>() > ENTRY_IS_TABLE);
const _: () = assert!(align_of::<StaticEncoderVtable<()>>() > ENTRY_IS_TABLE);

impl ErasedEncoderEntry {
    /// Decodes the entry into either a vtable or a table.
    ///
    /// # Safety
    /// The caller must ensure that the encoder was for resource type `R`.
    unsafe fn decode<R>(&self) -> Result<&StaticEncoderVtable<R>, &EncoderTable> {
        // SAFETY: `R` is guaranteed by caller to be the right type.
        unsafe {
            if self.0 as usize & ENTRY_IS_TABLE == 0 {
                Ok(&*self.0.cast::<StaticEncoderVtable<R>>())
            } else {
                Err(&*self
                    .0
                    .wrapping_byte_sub(ENTRY_IS_TABLE)
                    .cast::<EncoderTable>())
            }
        }
    }

    /// Writes a value to a field using the encoder, taking ownership of `field`.
    ///
    /// # Safety
    /// The caller must ensure that `field` points to a valid object of type `T`, and
    /// that the encoder is correct for `T` and `R`.
    pub unsafe fn write_field<R>(&self, field: *mut u8, writer: FieldWriter<'_, '_, R>) {
        // SAFETY: caller guarantees this encoder is correct for `T` and `R` and
        // that `field` points to a valid object of type `T`.
        unsafe {
            match self.decode::<R>() {
                Ok(vtable) => (vtable.write_fn)(field, writer),
                Err(table) => {
                    write_message_by_ptr(
                        table.count,
                        table.numbers,
                        table.encoders,
                        table.offsets,
                        field,
                        writer,
                    );
                }
            }
        }
    }

    /// Computes the size of a field using the encoder.
    ///
    /// # Safety
    /// The caller must ensure that `field` points to a valid object of type `T`, and
    /// that the encoder is correct for `T` and `R`.
    pub unsafe fn compute_size_field<R>(&self, field: *mut u8, sizer: FieldSizer<'_>) {
        // SAFETY: caller guarantees this encoder is correct for `T` and `R` and
        // that `field` points to a valid object of type `T`.
        unsafe {
            match self.decode::<R>() {
                Ok(vtable) => (vtable.compute_size_fn)(field, sizer),
                Err(table) => {
                    compute_size_message_by_ptr::<R>(
                        table.count,
                        table.numbers,
                        table.encoders,
                        table.offsets,
                        field,
                        sizer,
                    );
                }
            }
        }
    }
}

struct EncoderTable {
    count: usize,
    numbers: *const u32,
    encoders: *const ErasedEncoderEntry,
    offsets: *const usize,
}

/// A vtable for encoding a message.
struct StaticEncoderVtable<R> {
    write_fn: unsafe fn(*mut u8, FieldWriter<'_, '_, R>),
    compute_size_fn: unsafe fn(*mut u8, FieldSizer<'_>),
}

unsafe fn write_field_dyn<T, R, E: FieldEncode<T, R>>(
    field: *mut u8,
    writer: FieldWriter<'_, '_, R>,
) {
    // SAFETY: caller guarantees that `field` points to a `T`, and this function
    // takes ownership of it.
    let field = unsafe { field.cast::<T>().read() };
    E::write_field(field, writer);
}

unsafe fn compute_size_field_dyn<T, R, E: FieldEncode<T, R>>(
    field: *mut u8,
    sizer: FieldSizer<'_>,
) {
    // SAFETY: caller guarantees that `field` points to a `T`.
    let field = unsafe { &mut *field.cast::<T>() };
    E::compute_field_size(field, sizer);
}
