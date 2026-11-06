// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Physical-disk counters, independent of mounted-volume performance IOCTLs.

#![cfg(windows)]
// UNSAFETY: Calls to PDH and decoding its caller-owned output buffers.
#![expect(unsafe_code)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::mem::align_of;
use std::mem::size_of;
use std::mem::size_of_val;
use std::ptr;
use std::slice;
use windows_sys::Win32::System::Performance::PDH_CSTATUS_NEW_DATA;
use windows_sys::Win32::System::Performance::PDH_CSTATUS_VALID_DATA;
use windows_sys::Win32::System::Performance::PDH_HCOUNTER;
use windows_sys::Win32::System::Performance::PDH_HQUERY;
use windows_sys::Win32::System::Performance::PDH_MORE_DATA;
use windows_sys::Win32::System::Performance::PDH_RAW_COUNTER;
use windows_sys::Win32::System::Performance::PDH_RAW_COUNTER_ITEM_W;
use windows_sys::Win32::System::Performance::PdhAddEnglishCounterW;
use windows_sys::Win32::System::Performance::PdhCloseQuery;
use windows_sys::Win32::System::Performance::PdhCollectQueryData;
use windows_sys::Win32::System::Performance::PdhGetRawCounterArrayW;
use windows_sys::Win32::System::Performance::PdhOpenQueryW;

/// A live query of cumulative I/O bytes for each PhysicalDisk performance instance.
///
/// The query is thread-affine. Instances are enumerated on each collection;
/// invalid per-instance values remain errors rather than becoming zero.
pub struct PhysicalDiskCounters {
    query: PDH_HQUERY,
    reads: PDH_HCOUNTER,
    writes: PDH_HCOUNTER,
}

/// One disk's unformatted byte counters, with independent validity for each direction.
pub struct DiskIoSample {
    /// The PDH instance name, such as `0 C:`.
    pub instance: String,
    /// Cumulative bytes read, or the provider's validity error.
    pub read_bytes: io::Result<u64>,
    /// Cumulative bytes written, or the provider's validity error.
    pub written_bytes: io::Result<u64>,
}

impl PhysicalDiskCounters {
    /// Opens a live query using language-neutral read/write byte-counter paths.
    pub fn new() -> io::Result<Self> {
        let mut query = ptr::null_mut();
        // SAFETY: The output pointer is valid; a null data source selects live counters.
        check(
            unsafe { PdhOpenQueryW(ptr::null(), 0, &mut query) },
            "PdhOpenQueryW",
        )?;
        let mut sampler = Self {
            query,
            reads: ptr::null_mut(),
            writes: ptr::null_mut(),
        };
        sampler.reads = sampler.add_counter(r"\PhysicalDisk(*)\Disk Read Bytes/sec")?;
        sampler.writes = sampler.add_counter(r"\PhysicalDisk(*)\Disk Write Bytes/sec")?;
        Ok(sampler)
    }

    fn add_counter(&self, path: &str) -> io::Result<PDH_HCOUNTER> {
        let path_utf16: Vec<_> = path.encode_utf16().chain([0]).collect();
        let mut counter = ptr::null_mut();
        check(
            // SAFETY: The query is live, the path is terminated, and the output pointer is valid.
            unsafe { PdhAddEnglishCounterW(self.query, path_utf16.as_ptr(), 0, &mut counter) },
            "PdhAddEnglishCounterW",
        )
        .map_err(|err| io::Error::other(format!("adding {path}: {err}")))?;
        Ok(counter)
    }

    /// Collects current per-disk values, excluding the aggregate `_Total` instance.
    ///
    /// Query failures return an error. Individual invalid counters retain their
    /// errors without discarding other disks or the opposite I/O direction.
    pub fn sample(&mut self) -> io::Result<Vec<DiskIoSample>> {
        // SAFETY: This thread owns the live query and all its counters.
        check(
            unsafe { PdhCollectQueryData(self.query) },
            "PdhCollectQueryData",
        )?;
        let reads = read_counter(self.reads).map_err(|err| {
            io::Error::other(format!("reading PhysicalDisk read-byte counters: {err}"))
        })?;
        let writes = read_counter(self.writes).map_err(|err| {
            io::Error::other(format!("reading PhysicalDisk write-byte counters: {err}"))
        })?;
        let names = reads.keys().chain(writes.keys()).collect::<BTreeSet<_>>();
        let mut samples = Vec::new();
        for name in names {
            if name.eq_ignore_ascii_case("_Total") {
                continue;
            }
            samples.push(DiskIoSample {
                instance: name.clone(),
                read_bytes: counter_bytes(reads.get(name)),
                written_bytes: counter_bytes(writes.get(name)),
            });
        }
        if samples.is_empty() {
            return Err(io::Error::other("PDH returned no PhysicalDisk instances"));
        }
        Ok(samples)
    }
}

impl Drop for PhysicalDiskCounters {
    fn drop(&mut self) {
        if !self.query.is_null() {
            // SAFETY: The query is owned by this sampler; closing it also removes its counters.
            let status = unsafe { PdhCloseQuery(self.query) };
            assert_eq!(status, 0, "failed to close owned PDH query: {status:#010x}");
        }
    }
}

fn check(status: u32, operation: &str) -> io::Result<()> {
    if status != 0 {
        return Err(io::Error::other(format!(
            "{operation} failed with PDH status {status:#010x}"
        )));
    }
    Ok(())
}

fn counter_bytes(counter: Option<&PDH_RAW_COUNTER>) -> io::Result<u64> {
    let counter = counter.ok_or_else(|| io::Error::other("counter instance is missing"))?;
    if !matches!(
        counter.CStatus,
        PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
    ) {
        return Err(io::Error::other(format!(
            "invalid counter data, PDH status {:#010x}",
            counter.CStatus
        )));
    }
    // These byte-rate counters are PERF_COUNTER_BULK_COUNT: FirstValue is the
    // cumulative byte count, not the formatted per-second rate.
    u64::try_from(counter.FirstValue)
        .map_err(|_| io::Error::other("negative cumulative byte count"))
}

fn read_counter(counter: PDH_HCOUNTER) -> io::Result<BTreeMap<String, PDH_RAW_COUNTER>> {
    for _ in 0..3 {
        let mut bytes = 0;
        let mut count = 0;
        // SAFETY: A null buffer with size zero requests the required allocation size.
        let status =
            unsafe { PdhGetRawCounterArrayW(counter, &mut bytes, &mut count, ptr::null_mut()) };
        if status != PDH_MORE_DATA {
            return Err(io::Error::other(format!(
                "sizing PDH counter array failed with status {status:#010x}"
            )));
        }
        if bytes == 0 {
            return Err(io::Error::other("PDH requested an empty counter buffer"));
        }
        // Zeroed words provide alignment and initialized trailing storage for
        // the names, without depending on struct padding being initialized.
        const { assert!(align_of::<PDH_RAW_COUNTER_ITEM_W>() <= align_of::<u64>()) };
        let mut buffer = vec![0u64; (bytes as usize).div_ceil(size_of::<u64>())];
        let capacity_bytes = size_of_val(buffer.as_slice());
        // SAFETY: The buffer is aligned and has at least `bytes` writable bytes.
        let status = unsafe {
            PdhGetRawCounterArrayW(counter, &mut bytes, &mut count, buffer.as_mut_ptr().cast())
        };
        if status == PDH_MORE_DATA {
            // Instance counts can change. Re-query the size rather than relying
            // on the size returned for an insufficient nonempty buffer.
            continue;
        }
        check(status, "PdhGetRawCounterArrayW")?;
        if bytes as usize > capacity_bytes
            || count as usize > bytes as usize / size_of::<PDH_RAW_COUNTER_ITEM_W>()
        {
            return Err(io::Error::other(
                "PDH returned an invalid counter array size",
            ));
        }
        let mut values = BTreeMap::new();
        // SAFETY: The successful call initialized `count` items, the allocation
        // is aligned, and their size was checked against the returned buffer.
        let items = unsafe {
            slice::from_raw_parts(
                buffer.as_ptr().cast::<PDH_RAW_COUNTER_ITEM_W>(),
                count as usize,
            )
        };
        for item in items {
            let name = instance_name(item.szName, &buffer, bytes as usize)?;
            if values.insert(name, item.RawValue).is_some() {
                return Err(io::Error::other(
                    "PDH returned duplicate disk instance names",
                ));
            }
        }
        return Ok(values);
    }
    Err(io::Error::other(
        "PDH disk instance list changed during all three buffer allocation attempts",
    ))
}

fn instance_name(name: *const u16, buffer: &[u64], bytes: usize) -> io::Result<String> {
    let start = buffer.as_ptr() as usize;
    let address = name as usize;
    if bytes > size_of_val(buffer) {
        return Err(io::Error::other(
            "PDH instance buffer exceeds its allocation",
        ));
    }
    let end = start + bytes;
    if address < start || address >= end || !address.is_multiple_of(align_of::<u16>()) {
        return Err(io::Error::other(
            "PDH instance name points outside the counter buffer",
        ));
    }
    // SAFETY: The word allocation is initialized and aligned, every bit pattern
    // is a valid u16, and `bytes` was checked against the allocation's size.
    let words =
        unsafe { slice::from_raw_parts(buffer.as_ptr().cast::<u16>(), bytes / size_of::<u16>()) };
    let words = &words[(address - start) / size_of::<u16>()..];
    let len = words
        .iter()
        .position(|&word| word == 0)
        .ok_or_else(|| io::Error::other("unterminated PDH instance name"))?;
    String::from_utf16(&words[..len])
        .map_err(|err| io::Error::other(format!("invalid UTF-16 in PDH instance name: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn raw_counter_validity_is_not_inferred_from_its_value() {
        let mut item = PDH_RAW_COUNTER_ITEM_W::default();
        for status in [PDH_CSTATUS_VALID_DATA, PDH_CSTATUS_NEW_DATA] {
            item.RawValue.CStatus = status;
            item.RawValue.FirstValue = 0;
            assert_eq!(counter_bytes(Some(&item.RawValue)).unwrap(), 0);
            item.RawValue.FirstValue = 123456789;
            assert_eq!(counter_bytes(Some(&item.RawValue)).unwrap(), 123456789);
        }
        item.RawValue.CStatus = 0xc0000bc6;
        assert!(counter_bytes(Some(&item.RawValue)).is_err());
        item.RawValue.CStatus = PDH_CSTATUS_VALID_DATA;
        item.RawValue.FirstValue = -1;
        assert!(counter_bytes(Some(&item.RawValue)).is_err());
        assert!(counter_bytes(None).is_err());
    }

    #[test]
    fn instance_names_are_bounded_by_the_returned_buffer() {
        let buffer = [u64::from_ne_bytes([b'0', 0, b' ', 0, b'C', 0, b':', 0]), 0];
        let name = buffer.as_ptr().cast::<u16>();
        assert_eq!(instance_name(name, &buffer, 16).unwrap(), "0 C:");
        assert!(instance_name(name, &buffer, 8).is_err());
        assert!(instance_name(name, &buffer, 17).is_err());
        assert!(instance_name(ptr::null(), &buffer, 16).is_err());
        let invalid_utf16 = [u64::from_ne_bytes([0, 0xd8, 0, 0, 0, 0, 0, 0])];
        assert!(instance_name(invalid_utf16.as_ptr().cast(), &invalid_utf16, 8).is_err());
    }
}
