// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Table-based decoding.

use super::StructMetadata;
use super::TableEncoder;
use crate::inplace::InplaceOption;
use crate::protobuf::FieldReader;
use crate::protobuf::MessageReader;
use crate::Error;
use crate::FieldDecode;
use crate::MessageDecode;
use alloc::slice;
use alloc::vec;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

/// Calls `f` on `item`, splitting the pointer and initialized flag out.
///
/// # Safety
///
/// The caller must ensure that on the return, the bool specifies the
/// initialized state of the item.
unsafe fn run_inplace<T, R>(
    item: &mut InplaceOption<'_, T>,
    f: impl FnOnce(*mut u8, &mut bool) -> R,
) -> R {
    let mut initialized = item.forget();
    let base = item.as_mut_ptr().cast::<u8>();
    let r = f(base, &mut initialized);
    if initialized {
        // SAFETY: the caller ensures that `item` is initialized.
        unsafe { item.set_init_unchecked() };
    }
    r
}

impl<'de, T, R> MessageDecode<'de, T, R> for TableEncoder
where
    T: StructDecodeMetadata<'de, R>,
{
    fn read_message(
        item: &mut InplaceOption<'_, T>,
        reader: MessageReader<'de, '_, R>,
    ) -> crate::Result<()> {
        // SAFETY: T guarantees that the metadata is valid.
        unsafe {
            run_inplace(item, |base, initialized| {
                read_fields(
                    T::NUMBERS,
                    T::DECODERS,
                    T::OFFSETS,
                    base,
                    initialized,
                    reader,
                )
            })
        }
    }
}

impl<'de, T, R> FieldDecode<'de, T, R> for TableEncoder
where
    T: StructDecodeMetadata<'de, R>,
{
    // Override the default implementation to use the table decoder directly.
    // This saves code size by avoiding extra stub functions and vtables.
    const ENTRY: DecoderEntry<'de, T, R> = DecoderEntry::table();

    fn read_field(
        item: &mut InplaceOption<'_, T>,
        reader: FieldReader<'de, '_, R>,
    ) -> crate::Result<()> {
        // SAFETY: T guarantees that the metadata is valid.
        unsafe {
            run_inplace(item, |base, initialized| {
                read_message(
                    T::NUMBERS,
                    T::DECODERS,
                    T::OFFSETS,
                    base,
                    initialized,
                    reader,
                )
            })
        }
    }

    fn default_field(item: &mut InplaceOption<'_, T>) -> crate::Result<()> {
        // SAFETY: T guarantees that the metadata is valid.
        unsafe {
            run_inplace(item, |base, initialized| {
                default_fields(T::DECODERS, T::OFFSETS, base, initialized)
            })
        }
    }
}

/// Read a field as a message from the provided field metadata.
///
/// # Safety
///
/// The caller must ensure that `base` points to a location that can be written
/// to, that `struct_initialized` is set correctly, and that the metadata is
/// correct and complete for the type of the object pointed to by `base`.
#[doc(hidden)] // only used publicly in mesh_derive
pub unsafe fn read_message<R>(
    numbers: &[u32],
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    struct_initialized: &mut bool,
    reader: FieldReader<'_, '_, R>,
) -> Result<(), Error> {
    assert_eq!(numbers.len(), decoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by caller and by the assertions above.
    unsafe {
        // Convert the slices to pointers and a single length to shrink
        // code size.
        read_message_by_ptr(
            numbers.len(),
            numbers.as_ptr(),
            decoders.as_ptr(),
            offsets.as_ptr(),
            base,
            struct_initialized,
            reader,
        )
    }
}

// Don't inline this since it is used by every table decoder instantiation.
#[inline(never)]
unsafe fn read_message_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    decoders: *const ErasedDecoderEntry,
    offsets: *const usize,
    base: *mut u8,
    struct_initialized: &mut bool,
    reader: FieldReader<'_, '_, R>,
) -> Result<(), Error> {
    // SAFETY: guaranteed by caller.
    unsafe {
        read_fields_inline(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(decoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            struct_initialized,
            reader.message()?,
        )
    }
}

/// Read a message from the provided field metadata.
///
/// # Safety
///
/// The caller must ensure that `base` points to a location that can be written
/// to, that `struct_initialized` is set correctly, and that the metadata is
/// correct and complete for the type of the object pointed to by `base`.
unsafe fn read_fields<R>(
    numbers: &[u32],
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    struct_initialized: &mut bool,
    reader: MessageReader<'_, '_, R>,
) -> Result<(), Error> {
    assert_eq!(numbers.len(), decoders.len());
    assert_eq!(numbers.len(), offsets.len());
    // SAFETY: guaranteed by caller and by the assertions above.
    unsafe {
        // Convert the slices to pointers and a single length to shrink
        // code size.
        read_fields_by_ptr(
            numbers.len(),
            numbers.as_ptr(),
            decoders.as_ptr(),
            offsets.as_ptr(),
            base,
            struct_initialized,
            reader,
        )
    }
}

// Don't inline this since it is used by every table decoder instantiation.
#[inline(never)]
unsafe fn read_fields_by_ptr<R>(
    count: usize,
    numbers: *const u32,
    decoders: *const ErasedDecoderEntry,
    offsets: *const usize,
    base: *mut u8,
    struct_initialized: &mut bool,
    reader: MessageReader<'_, '_, R>,
) -> Result<(), Error> {
    // SAFETY: guaranteed by caller.
    unsafe {
        read_fields_inline(
            slice::from_raw_parts(numbers, count),
            slice::from_raw_parts(decoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            struct_initialized,
            reader,
        )
    }
}

/// Reads fields from the provided field metadata.
///
/// # Safety
///
/// The caller must ensure that `base` points to a location that can be written
/// to, that `initialized` is set correctly, and that the metadata is correct
/// and complete for the type of the object pointed to by `base`.
unsafe fn read_fields_inline<R>(
    numbers: &[u32],
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    struct_initialized: &mut bool,
    reader: MessageReader<'_, '_, R>,
) -> Result<(), Error> {
    const STACK_LIMIT: usize = 32;
    let mut field_init_static;
    let mut field_init_dynamic;
    let field_inits = if numbers.len() <= STACK_LIMIT {
        field_init_static = [false; STACK_LIMIT];
        field_init_static[..numbers.len()].fill(*struct_initialized);
        &mut field_init_static[..numbers.len()]
    } else {
        field_init_dynamic = vec![*struct_initialized; numbers.len()];
        &mut field_init_dynamic[..]
    };

    // SAFETY: guaranteed by caller.
    let r = unsafe { read_fields_inner(numbers, decoders, offsets, base, field_inits, reader) };
    *struct_initialized = true;
    if r.is_err() && !field_inits.iter().all(|&b| b) {
        // Drop any initialized fields.
        for ((field_init, &offset), decoder) in field_inits.iter_mut().zip(offsets).zip(decoders) {
            if *field_init {
                // SAFETY: guaranteed by the caller.
                unsafe {
                    decoder.drop_field(base.add(offset));
                }
            }
        }
        *struct_initialized = false;
    }
    r
}

/// Reads fields from the provided field metadata, but does not drop any fields
/// of a partially initialized message on failure.
unsafe fn read_fields_inner<R>(
    numbers: &[u32],
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    field_init: &mut [bool],
    reader: MessageReader<'_, '_, R>,
) -> Result<(), Error> {
    let decoders = &decoders[..numbers.len()];
    let offsets = &offsets[..numbers.len()];
    let field_init = &mut field_init[..numbers.len()];
    for field in reader {
        let (number, reader) = field?;
        if let Some(index) = numbers.iter().position(|&n| n == number) {
            let decoder = &decoders[index];
            // SAFETY: the decoder is valid according to the caller.
            unsafe {
                decoder.read_field(base.add(offsets[index]), &mut field_init[index], reader)?;
            }
        }
    }
    for ((field_init, &offset), decoder) in field_init.iter_mut().zip(offsets).zip(decoders) {
        if !*field_init {
            // SAFETY: the decoder is valid according to the caller.
            unsafe {
                decoder.default_field(base.add(offset), field_init)?;
            }
            assert!(*field_init);
        }
    }
    Ok(())
}

/// Sets fields to their default values from the provided field metadata.
///
/// # Safety
///
/// The caller must ensure that `base` points to a location that can be written
/// to, that `struct_initialized` is set correctly, and that the metadata is
/// correct and complete for the type of the object pointed to by `base`.
unsafe fn default_fields(
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    struct_initialized: &mut bool,
) -> Result<(), Error> {
    assert_eq!(decoders.len(), offsets.len());
    // SAFETY: guaranteed by caller and by the assertion above.
    unsafe {
        default_fields_by_ptr(
            decoders.len(),
            decoders.as_ptr(),
            offsets.as_ptr(),
            base,
            struct_initialized,
        )
    }
}

#[inline(never)]
unsafe fn default_fields_by_ptr(
    count: usize,
    decoders: *const ErasedDecoderEntry,
    offsets: *const usize,
    base: *mut u8,
    struct_initialized: &mut bool,
) -> Result<(), Error> {
    // SAFETY: guaranteed by caller.
    unsafe {
        default_fields_inline(
            slice::from_raw_parts(decoders, count),
            slice::from_raw_parts(offsets, count),
            base,
            struct_initialized,
        )
    }
}

unsafe fn default_fields_inline(
    decoders: &[ErasedDecoderEntry],
    offsets: &[usize],
    base: *mut u8,
    struct_initialized: &mut bool,
) -> Result<(), Error> {
    for (i, (&offset, decoder)) in offsets.iter().zip(decoders).enumerate() {
        let mut field_initialized = *struct_initialized;
        // SAFETY: the decoder is valid according to the caller.
        let r = unsafe { decoder.default_field(base.add(offset), &mut field_initialized) };
        if let Err(err) = r {
            if !field_initialized || !*struct_initialized {
                // Drop initialized fields.
                let initialized_until = i;
                for (i, (&offset, decoder)) in offsets.iter().zip(decoders).enumerate() {
                    if i < initialized_until
                        || (i == initialized_until && field_initialized)
                        || (i > initialized_until && *struct_initialized)
                    {
                        // SAFETY: the decoder is valid according to the caller, and the field is initialized.
                        unsafe {
                            decoder.drop_field(base.add(offset));
                        }
                    }
                }
                *struct_initialized = false;
            }
            return Err(err);
        }
        assert!(field_initialized);
    }
    *struct_initialized = true;
    Ok(())
}

/// The struct metadata for decoding a struct.
///
/// # Safety
///
/// The implementor must ensure that the `DECODERS` are correct and complete for
/// `Self`, such that if every field is decoded, then the struct value is valid.
pub unsafe trait StructDecodeMetadata<'de, R>: StructMetadata {
    /// The list of decoder vtables.
    const DECODERS: &'static [ErasedDecoderEntry];
}

/// An entry in the decoder table.
///
/// This contains the metadata necessary to apply an decoder to a field.
///
/// This cannot be instantiated directly; use [`FieldDecode::ENTRY`] to get an
/// instance for a particular decoder.
pub struct DecoderEntry<'a, T, R>(
    ErasedDecoderEntry,
    PhantomData<fn(&mut T, &mut R, &'a mut ())>,
);

impl<'a, T, R> DecoderEntry<'a, T, R> {
    /// # Safety
    /// The caller must ensure that the erased entry is an valid entry for `T`
    /// and `R`.
    pub(crate) const unsafe fn new_unchecked(entry: ErasedDecoderEntry) -> Self {
        Self(entry, PhantomData)
    }

    pub(crate) const fn custom<E: FieldDecode<'a, T, R>>() -> Self {
        Self(
            ErasedDecoderEntry(
                core::ptr::from_ref(
                    const {
                        &StaticDecoderVtable {
                            read_fn: read_field_dyn::<T, R, E>,
                            default_fn: default_field_dyn::<T, R, E>,
                            drop_fn: if core::mem::needs_drop::<T>() {
                                Some(drop_field_dyn::<T>)
                            } else {
                                None
                            },
                        }
                    },
                )
                .cast(),
            ),
            PhantomData,
        )
    }

    const fn table() -> Self
    where
        T: StructDecodeMetadata<'a, R>,
    {
        Self(
            ErasedDecoderEntry(
                core::ptr::from_ref(
                    const {
                        &DecoderTable {
                            count: T::NUMBERS.len(),
                            numbers: T::NUMBERS.as_ptr(),
                            decoders: T::DECODERS.as_ptr(),
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

    /// Erases the type of the decoder entry.
    pub const fn erase(&self) -> ErasedDecoderEntry {
        self.0
    }
}

/// An entry in a [`StructDecodeMetadata::DECODERS`] table.
//
// Internally, this is a pointer to either a vtable or a table.
// The low bit is used to distinguish between the two.
#[derive(Copy, Clone, Debug)]
pub struct ErasedDecoderEntry(*const ());

// SAFETY: the entry represents a set of integers and function pointers, which
// have no cross-thread constraints.
unsafe impl Send for ErasedDecoderEntry {}
// SAFETY: the entry represents a set of integers and function pointers, which
// have no cross-thread constraints.
unsafe impl Sync for ErasedDecoderEntry {}

const ENTRY_IS_TABLE: usize = 1;

const _: () = assert!(align_of::<ErasedDecoderEntry>() > ENTRY_IS_TABLE);
const _: () = assert!(align_of::<StaticDecoderVtable<'_, ()>>() > ENTRY_IS_TABLE);

impl ErasedDecoderEntry {
    /// Decodes the entry into either a vtable or a table.
    ///
    /// # Safety
    /// The caller must ensure that the encoder was for resource type `R`.
    unsafe fn decode<'de, R>(&self) -> Result<&StaticDecoderVtable<'de, R>, &DecoderTable> {
        // SAFETY: guaranteed by caller.
        unsafe {
            if self.0 as usize & ENTRY_IS_TABLE == 0 {
                Ok(&*self.0.cast::<StaticDecoderVtable<'_, R>>())
            } else {
                Err(&*self
                    .0
                    .wrapping_byte_sub(ENTRY_IS_TABLE)
                    .cast::<DecoderTable>())
            }
        }
    }

    /// Reads a field using the decoder metadata.
    ///
    /// # Safety
    /// The caller must ensure that the decoder was for resource type `R` and
    /// the object type matches what `ptr` is pointing to. `*init` must be set
    /// if and only if the field is initialized.
    pub unsafe fn read_field<R>(
        &self,
        ptr: *mut u8,
        init: &mut bool,
        reader: FieldReader<'_, '_, R>,
    ) -> Result<(), Error> {
        // SAFETY: guaranteed by caller.
        unsafe {
            match self.decode::<R>() {
                Ok(vtable) => (vtable.read_fn)(ptr, init, reader),
                Err(table) => read_message_by_ptr(
                    table.count,
                    table.numbers,
                    table.decoders,
                    table.offsets,
                    ptr,
                    init,
                    reader,
                ),
            }
        }
    }

    /// Initializes a value to its default state using the decoder metadata.
    ///
    /// # Safety
    /// The caller must ensure that the decoder was for the object type matching
    /// what `ptr` is pointing to.
    pub unsafe fn default_field(&self, ptr: *mut u8, init: &mut bool) -> Result<(), Error> {
        // SAFETY: guaranteed by caller.
        unsafe {
            match self.decode::<()>() {
                Ok(vtable) => (vtable.default_fn)(ptr, init),
                Err(table) => {
                    default_fields_by_ptr(table.count, table.decoders, table.offsets, ptr, init)
                }
            }
        }
    }

    /// Drops a value in place using the decoder metadata.
    ///
    /// # Safety
    /// The caller must ensure that the decoder was for the object type matching
    /// what `ptr` is pointing to, and that `ptr` is ready to be dropped.
    pub unsafe fn drop_field(&self, ptr: *mut u8) {
        // SAFETY: guaranteed by caller.
        unsafe {
            match self.decode::<()>() {
                Ok(vtable) => {
                    if let Some(drop_fn) = vtable.drop_fn {
                        drop_fn(ptr);
                    }
                }
                Err(table) => {
                    for i in 0..table.count {
                        let offset = *table.offsets.add(i);
                        let decoder = &*table.decoders.add(i);
                        decoder.drop_field(ptr.add(offset));
                    }
                }
            }
        }
    }
}

struct DecoderTable {
    count: usize,
    numbers: *const u32,
    decoders: *const ErasedDecoderEntry,
    offsets: *const usize,
}

/// A vtable for decoding a message.
#[repr(C)] // to ensure the layout is the same regardless of R
struct StaticDecoderVtable<'de, R> {
    read_fn: unsafe fn(*mut u8, init: *mut bool, FieldReader<'de, '_, R>) -> Result<(), Error>,
    default_fn: unsafe fn(*mut u8, init: *mut bool) -> Result<(), Error>,
    drop_fn: Option<unsafe fn(*mut u8)>,
}

unsafe fn read_field_dyn<'a, T, R, E: FieldDecode<'a, T, R>>(
    field: *mut u8,
    init: *mut bool,
    reader: FieldReader<'a, '_, R>,
) -> Result<(), Error> {
    // SAFETY: `init` is valid according to the caller.
    let init = unsafe { &mut *init };
    // SAFETY: `field` is valid and points to a valid `MaybeUninit<T>` according
    // to the caller.
    let field = unsafe { &mut *field.cast::<MaybeUninit<T>>() };
    let mut field = if *init {
        // SAFETY: the caller attests that the field is initialized.
        unsafe { InplaceOption::new_init_unchecked(field) }
    } else {
        InplaceOption::uninit(field)
    };
    let r = E::read_field(&mut field, reader);
    *init = field.forget();
    r
}

unsafe fn default_field_dyn<'a, T, R, E: FieldDecode<'a, T, R>>(
    field: *mut u8,
    init: *mut bool,
) -> Result<(), Error> {
    // SAFETY: `init` is valid according to the caller.
    let init = unsafe { &mut *init };
    // SAFETY: `field` is valid and points to a valid `MaybeUninit<T>` according
    // to the caller.
    let field = unsafe { &mut *field.cast::<MaybeUninit<T>>() };
    let mut field = if *init {
        // SAFETY: the caller attests that the field is initialized.
        unsafe { InplaceOption::new_init_unchecked(field) }
    } else {
        InplaceOption::uninit(field)
    };
    let r = E::default_field(&mut field);
    *init = field.forget();
    r
}

unsafe fn drop_field_dyn<T>(field: *mut u8) {
    let field = field.cast::<T>();
    // SAFETY: `field` is valid and points to a valid `T` according to the
    // caller.
    unsafe { field.drop_in_place() }
}
