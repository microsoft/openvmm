// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for implementing device emulators.

/// Performs a device register read as a series of 32-bit reads.
pub fn read_as_u32_chunks<F, Num>(offset: Num, data: &mut [u8], mut read_u32: F)
where
    F: FnMut(Num) -> u32,
    Num: Into<u64> + TryFrom<u64>,
    <Num as TryFrom<u64>>::Error: std::fmt::Debug,
{
    let offset = offset.into();
    let mut next_offset = offset;
    let remaining_data = if offset & 3 != 0 {
        let val = read_u32((offset & !3).try_into().unwrap());
        let val = val.to_ne_bytes();
        let u32_offset = (offset & 3) as usize;
        let byte_count = std::cmp::min(4 - u32_offset, data.len());
        data[..byte_count].copy_from_slice(&val[u32_offset..(byte_count + u32_offset)]);
        next_offset += u64::try_from(byte_count).unwrap();
        let (_, rem) = data.split_at_mut(byte_count);
        rem
    } else {
        data
    };

    for next_chunk in remaining_data.chunks_exact_mut(4) {
        let val = read_u32(next_offset.try_into().unwrap());
        next_offset += 4;
        next_chunk.copy_from_slice(&val.to_ne_bytes());
    }
    let extra_bytes = remaining_data.chunks_exact_mut(4).into_remainder();
    if !extra_bytes.is_empty() {
        let val = read_u32(next_offset.try_into().unwrap());
        let val = val.to_ne_bytes();
        extra_bytes.copy_from_slice(&val[..extra_bytes.len()]);
    }
}

/// The request type for [`write_as_u32_chunks`].
pub enum ReadWriteRequestType {
    /// A read request.
    Read,
    /// A write request with the given value.
    Write(u32),
}

/// Performs a device register write as a series of 32-bit reads and writes.
///
/// NOTE: We read u32 and then write back when we chunk.  Because of this, the borrow checker
///       requires a single mutable closure that implements both read/write semantics.
pub fn write_as_u32_chunks<F, Num>(offset: Num, data: &[u8], mut read_write_u32: F)
where
    F: FnMut(Num, ReadWriteRequestType) -> Option<u32>,
    Num: Into<u64> + TryFrom<u64>,
    <Num as TryFrom<u64>>::Error: std::fmt::Debug,
{
    let offset = offset.into();
    let mut next_offset = offset;
    let remaining_data = if next_offset & 3 != 0 {
        let val = read_write_u32(
            (next_offset & !3).try_into().unwrap(),
            ReadWriteRequestType::Read,
        )
        .expect("Read for ReadWriteFn didn't return u32");
        let mut val = val.to_ne_bytes();
        let u32_offset = (next_offset & 3) as usize;
        let byte_count = std::cmp::min(4 - u32_offset, data.len());
        val[u32_offset..(byte_count + u32_offset)].copy_from_slice(&data[..byte_count]);
        next_offset += u64::try_from(byte_count).unwrap();
        read_write_u32(
            (offset & !3).try_into().unwrap(),
            ReadWriteRequestType::Write(u32::from_ne_bytes(val)),
        );
        let (_, rem) = data.split_at(byte_count);
        rem
    } else {
        data
    };
    for next_chunk in remaining_data.chunks_exact(4) {
        let val = u32::from_ne_bytes(
            next_chunk
                .try_into()
                .expect("4 byte chunk should convert to u32"),
        );
        read_write_u32(
            next_offset.try_into().unwrap(),
            ReadWriteRequestType::Write(val),
        );
        next_offset += 4;
    }
    let extra_bytes = remaining_data.chunks_exact(4).remainder();
    if !extra_bytes.is_empty() {
        let val = read_write_u32(next_offset.try_into().unwrap(), ReadWriteRequestType::Read)
            .expect("Read for ReadWriteFn didn't return u32");
        let mut val = val.to_ne_bytes();
        for (i, &extra_data) in extra_bytes.iter().enumerate() {
            val[i] = extra_data;
        }
        let val = u32::from_ne_bytes(val);
        read_write_u32(
            next_offset.try_into().unwrap(),
            ReadWriteRequestType::Write(val),
        );
    }
}
