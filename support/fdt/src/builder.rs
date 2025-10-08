// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to generate a Flattened DeviceTree binary blob.

use crate::spec;
use core::marker::PhantomData;
use core::mem::size_of;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// The FDT builder.
///
/// Uses the default `BubbleSortValidator` validator of the quadratic time
/// complexity to validate memory reservations. That is suitable for
/// the small number of memory reservations (say, up until ~8).
/// Consider implementing your own validator if you expect a larger number
/// of memory reservations.
pub struct Builder<'a, T = (), V = BubbleSortValidator>
where
    V: MemoryReservationValidator,
{
    inner: Inner<'a>,
    _phantom: PhantomData<(fn(T), V)>,
}

struct Inner<'a> {
    buffer: &'a mut [u8],
    memory_reservations: &'a [spec::ReserveEntry],
    memory_reservations_off: usize,
    string_table_off: usize,
    string_table_size: usize,
    string_table_cap: usize,
    struct_table_off: usize,
    struct_table_size: usize,
}

/// A string ID in the string table.
#[derive(Debug, Copy, Clone)]
pub struct StringId(spec::U32b);

/// Errors returned by the FDT builder.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum Error {
    /// No space left in buffer.
    #[error("out of space")]
    OutOfSpace,
    /// Memory reservations overlap.
    #[error("memory reservations overlap: {0:?} and {1:?}")]
    OverlappingMemoryReservations(spec::ReserveEntry, spec::ReserveEntry),
    /// Duplicate memory reservation.
    #[error("duplicate memory reservation: {0:?}")]
    DuplicateMemoryReservations(spec::ReserveEntry),
    /// Zero-sized memory reservation.
    #[error("zero-sized memory reservation")]
    ZeroMemoryReservation,
}

/// Type used to track node nesting level.
#[derive(Debug)]
pub struct Nest<T>(PhantomData<fn(T)>);

/// Trait for validating memory reservations.
pub trait MemoryReservationValidator {
    /// Validate memory reservations.
    fn validate_memory_reservations(
        memory_reservations: &[spec::ReserveEntry],
    ) -> Result<(), Error>;
}

/// An O(n^2) algorithm to check for overlapping memory reservations.
/// This is not a problem in practice because the number of memory reservations
/// is expected to be small (typically 1 or 2).
///
/// Any O(n log n) algorithm would require additional memory allocations, which
/// is not possible in a no-std environment without a dependency on an allocator.
///
/// Up until ~8 entries, the O(n^2) algorithm is not much worse than the O(n log n) one,
/// and has all the chances to be twice as worse for ~16 entries.
pub struct BubbleSortValidator;

impl MemoryReservationValidator for BubbleSortValidator {
    /// Validate memory reservations.
    fn validate_memory_reservations(
        memory_reservations: &[spec::ReserveEntry],
    ) -> Result<(), Error> {
        let entry_is_empty =
            |&spec::ReserveEntry { address, size }| u64::from(address) == 0 && u64::from(size) == 0;
        let validate_entries =
            |entry1: &spec::ReserveEntry, entry2: &spec::ReserveEntry| -> Result<(), Error> {
                let base1 = u64::from(entry1.address);
                let base2 = u64::from(entry2.address);
                let size1 = u64::from(entry1.size);
                let size2 = u64::from(entry2.size);

                if base1 == base2 && size1 == size2 {
                    return Err(Error::DuplicateMemoryReservations(*entry1));
                }

                if base1 < base2 + size2 && base2 < base1 + size1 {
                    return Err(Error::OverlappingMemoryReservations(*entry1, *entry2));
                }

                Ok(())
            };

        for (current_idx, entry1) in memory_reservations.iter().enumerate() {
            if entry_is_empty(entry1) {
                return Err(Error::ZeroMemoryReservation);
            }
            for entry2 in &memory_reservations[current_idx + 1..] {
                validate_entries(entry1, entry2)?;
            }
        }

        Ok(())
    }
}

/// FDT builder configuration.
///
/// There is no default or much of the optional configuration, so
/// the Rust Builder pattern is not used.
pub struct BuilderConfig<'a> {
    /// A buffer to store the FDT blob.
    pub blob_buffer: &'a mut [u8],
    /// The capacity of the string table.
    pub string_table_cap: usize,
    /// The memory reservations, a list of (address, size) pairs
    /// that goes into the `/memreserve/` node.
    /// The entries must not overlap and must not be (0, 0).
    pub memory_reservations: &'a [spec::ReserveEntry],
}

impl<'a, V> Builder<'a, (), V>
where
    V: MemoryReservationValidator,
{
    /// Creates a new builder.
    pub fn new(
        BuilderConfig {
            blob_buffer: buffer,
            string_table_cap,
            memory_reservations,
        }: BuilderConfig<'a>,
    ) -> Result<Self, Error> {
        V::validate_memory_reservations(memory_reservations)?;

        // At least one memory reservation entry is required: the sentinel value of (0, 0).
        let memory_reservations_size =
            (memory_reservations.len() + 1) * size_of::<spec::ReserveEntry>();
        let memory_reservations_off = size_of::<spec::Header>();
        let string_table_off = memory_reservations_off + memory_reservations_size;
        let struct_table_off = string_table_off + string_table_cap;
        Ok(Self {
            inner: Inner {
                buffer,
                memory_reservations,
                memory_reservations_off,
                string_table_size: 0,
                string_table_cap,
                string_table_off,
                struct_table_off,
                struct_table_size: 0,
            },
            _phantom: PhantomData,
        })
    }

    /// Finishes building the FDT blob. Returns the number of bytes used.
    pub fn build(mut self, boot_cpuid_phys: u32) -> Result<usize, Error> {
        // End the struct table.
        self.inner.write_struct(&spec::END.to_be_bytes())?;

        // Write the reserve map.
        self.inner
            .memory_reservations
            .as_bytes()
            .write_to_prefix(
                self.inner
                    .buffer
                    .get_mut(
                        self.inner.memory_reservations_off
                            ..self.inner.memory_reservations_off
                                + size_of_val(self.inner.memory_reservations),
                    )
                    .ok_or(Error::OutOfSpace)?,
            )
            .map_err(|_| Error::OutOfSpace)?;

        // Write the required empty reservation entry to end the reserve map.
        spec::ReserveEntry {
            address: 0.into(),
            size: 0.into(),
        }
        .write_to_prefix(
            self.inner
                .buffer
                .get_mut(
                    self.inner.string_table_off - size_of::<spec::ReserveEntry>()
                        ..self.inner.string_table_off,
                )
                .ok_or(Error::OutOfSpace)?,
        )
        .map_err(|_| Error::OutOfSpace)?;

        // Determine how many bytes were used. The struct table is the last
        // thing stored in buffer.
        let total_size = self.inner.struct_table_off + self.inner.struct_table_size;
        assert!(total_size <= self.inner.buffer.len());

        let header = spec::Header {
            magic: spec::MAGIC.into(),
            totalsize: (total_size as u32).into(),
            off_dt_struct: (self.inner.struct_table_off as u32).into(),
            off_dt_strings: (self.inner.string_table_off as u32).into(),
            off_mem_rsvmap: (self.inner.memory_reservations_off as u32).into(),
            version: spec::CURRENT_VERSION.into(),
            last_comp_version: spec::COMPAT_VERSION.into(),
            boot_cpuid_phys: boot_cpuid_phys.into(),
            size_dt_struct: (self.inner.struct_table_size as u32).into(),
            size_dt_strings: (self.inner.string_table_size as u32).into(),
        };
        header
            .write_to_prefix(self.inner.buffer)
            .map_err(|_| Error::OutOfSpace)?;

        Ok(total_size)
    }
}

impl<'a, T> Builder<'a, T> {
    /// Starts a new child node.
    pub fn start_node(mut self, name: &str) -> Result<Builder<'a, Nest<T>>, Error> {
        self.inner.write_struct(&spec::BEGIN_NODE.to_be_bytes())?;
        self.inner.write_struct(name.as_bytes())?;
        self.inner.write_struct(&[0])?;
        self.inner.align_struct()?;
        Ok(Builder {
            inner: self.inner,
            _phantom: PhantomData,
        })
    }

    /// Adds a string to the string table.
    pub fn add_string(&mut self, s: &str) -> Result<StringId, Error> {
        let len = s.len() + 1;
        if self.inner.string_table_size + len > self.inner.string_table_cap {
            return Err(Error::OutOfSpace);
        }

        let off = self.inner.string_table_off + self.inner.string_table_size;
        self.inner.buffer[off..off + s.len()].copy_from_slice(s.as_bytes());
        self.inner.buffer[off + s.len()] = 0;
        let id = StringId((self.inner.string_table_size as u32).into());
        self.inner.string_table_size += len;
        Ok(id)
    }
}

impl<'a, T> Builder<'a, Nest<T>> {
    /// Adds a property that does not have a value.
    pub fn add_null(mut self, name: StringId) -> Result<Self, Error> {
        self.inner.prop(name, &[])?;
        Ok(self)
    }

    /// Adds a u32 property.
    pub fn add_u32(mut self, name: StringId, data: u32) -> Result<Self, Error> {
        self.inner.prop(name, spec::U32b::new(data).as_bytes())?;
        Ok(self)
    }

    /// Adds a u64 property.
    pub fn add_u64(mut self, name: StringId, data: u64) -> Result<Self, Error> {
        self.inner.prop(name, spec::U64b::new(data).as_bytes())?;
        Ok(self)
    }

    /// Adds an array of u64 properties. Useful for `reg` or `ranges`.
    pub fn add_u64_array(mut self, name: StringId, data: &[u64]) -> Result<Self, Error> {
        let data = data.iter().map(|val| val.to_be_bytes());
        self.inner.prop_array_iter(name, data)?;
        Ok(self)
    }

    /// Adds a list of u64 properties. Useful for `reg` or `ranges`.
    pub fn add_u64_list(
        mut self,
        name: StringId,
        data: impl IntoIterator<Item = u64>,
    ) -> Result<Self, Error> {
        let data = data.into_iter().map(|val| val.to_be_bytes());
        self.inner.prop_array_iter(name, data)?;
        Ok(self)
    }

    /// Adds an array of u32 properties.
    pub fn add_u32_array(mut self, name: StringId, data: &[u32]) -> Result<Self, Error> {
        let data = data.iter().map(|val| val.to_be_bytes());
        self.inner.prop_array_iter(name, data)?;
        Ok(self)
    }

    /// Adds an array of properties. The caller must ensure these are Big Endian
    /// slices.
    pub fn add_prop_array(mut self, name: StringId, data_array: &[&[u8]]) -> Result<Self, Error> {
        self.inner.prop_array_iter(name, data_array.iter())?;
        Ok(self)
    }

    /// Adds a string property.
    pub fn add_str(mut self, name: StringId, data: &str) -> Result<Self, Error> {
        self.inner
            .prop_array_iter(name, [data.as_bytes(), &[0]].iter())?;
        Ok(self)
    }

    /// Adds a string list property.
    pub fn add_str_array(mut self, name: StringId, strings: &[&str]) -> Result<Self, Error> {
        self.inner
            .prop_array_iter(name, StringBytesWithZeroIter::new(strings))?;
        Ok(self)
    }

    /// Ends this node.
    pub fn end_node(mut self) -> Result<Builder<'a, T>, Error> {
        self.inner.write_struct(&spec::END_NODE.to_be_bytes())?;
        Ok(Builder {
            inner: self.inner,
            _phantom: PhantomData,
        })
    }
}

impl Inner<'_> {
    fn write_struct(&mut self, b: &[u8]) -> Result<usize, Error> {
        let off = self.struct_table_off + self.struct_table_size;
        self.buffer
            .get_mut(off..off + b.len())
            .ok_or(Error::OutOfSpace)?
            .copy_from_slice(b);
        self.struct_table_size += b.len();
        Ok(off)
    }

    fn align_struct(&mut self) -> Result<(), Error> {
        let new_size = (self.struct_table_size + 3) & !3;
        if self.struct_table_off + new_size <= self.buffer.len() {
            self.struct_table_size = new_size;
            Ok(())
        } else {
            Err(Error::OutOfSpace)
        }
    }

    /// Vector-valued property.
    /// Some DeviceTree nodes (such as `reg`, `ranges`, `dma-ranges`, ...)
    /// might expect several values present in the value when used for declaring
    /// busses. The specification calls the type of the value "prop-encoded-array".
    fn prop_array_iter(
        &mut self,
        name: StringId,
        data_iter: impl Iterator<Item = impl AsRef<[u8]>>,
    ) -> Result<(), Error> {
        self.write_struct(&spec::PROP.to_be_bytes())?;
        let header_offset = self.write_struct(spec::PropHeader::new_zeroed().as_bytes())?;
        let mut n = 0;
        for data in data_iter {
            let data = data.as_ref();
            n += data.len();
            self.write_struct(data)?;
        }
        // Write the header now that we know the length.
        spec::PropHeader {
            len: (n as u32).into(),
            nameoff: name.0,
        }
        .write_to_prefix(&mut self.buffer[header_offset..])
        .unwrap(); // PANIC: May panic if the buffer is too small (a programming error).
        self.align_struct()?;
        Ok(())
    }

    /// Scalar-valued property.
    fn prop(&mut self, name: StringId, data: &[u8]) -> Result<(), Error> {
        self.prop_array_iter(name, [data].iter())?;
        Ok(())
    }
}

#[derive(Clone)]
struct StringBytesWithZeroIter<'a> {
    strings: core::slice::Iter<'a, &'a str>,
    send_zero: bool,
}

impl<'a> StringBytesWithZeroIter<'a> {
    fn new(strings: &'a [&'a str]) -> Self {
        Self {
            strings: strings.iter(),
            send_zero: strings.is_empty(),
        }
    }
}

impl<'a> Iterator for StringBytesWithZeroIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.send_zero {
            self.send_zero = false;
            return Some(&[0]);
        }

        if let Some(s) = self.strings.next() {
            self.send_zero = true;
            return Some(s.as_bytes());
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_entry(address: u64, size: u64) -> spec::ReserveEntry {
        spec::ReserveEntry {
            address: address.into(),
            size: size.into(),
        }
    }

    #[test]
    fn test_overlapping_memory_reservations() {
        let entry1 = create_entry(100, 50);
        let entry2 = create_entry(120, 30);
        let reservations = [entry1, entry2];

        assert_eq!(
            BubbleSortValidator::validate_memory_reservations(&reservations),
            Err(Error::OverlappingMemoryReservations(entry1, entry2))
        );
    }

    #[test]
    fn test_duplicate_memory_reservations() {
        let entry1 = create_entry(100, 50);
        let entry2 = create_entry(100, 50);
        let reservations = [entry1, entry2];

        assert_eq!(
            BubbleSortValidator::validate_memory_reservations(&reservations),
            Err(Error::DuplicateMemoryReservations(entry1))
        );
    }

    #[test]
    fn test_zero_memory_reservation() {
        let entry = create_entry(0, 0);
        let reservations = [entry];

        assert_eq!(
            BubbleSortValidator::validate_memory_reservations(&reservations),
            Err(Error::ZeroMemoryReservation)
        );
    }

    #[test]
    fn test_valid_memory_reservations() {
        let entry1 = create_entry(100, 50);
        let entry2 = create_entry(200, 50);
        let reservations = [entry1, entry2];

        assert_eq!(
            BubbleSortValidator::validate_memory_reservations(&reservations),
            Ok(())
        );
    }

    #[test]
    fn test_valid_adjacent_memory_reservations() {
        let entry1 = create_entry(100, 50);
        let entry2 = create_entry(150, 50);
        let reservations = [entry1, entry2];

        assert_eq!(
            BubbleSortValidator::validate_memory_reservations(&reservations),
            Ok(())
        );
    }
}
