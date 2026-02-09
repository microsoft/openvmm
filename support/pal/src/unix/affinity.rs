// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thread affinity support for Linux.

#![cfg(target_os = "linux")]

use std::io;
use std::sync::OnceLock;
use thiserror::Error;

/// A [`libc::cpu_set_t`] sized appropriately to the number of processors on
/// this machine.
///
/// This is needed to support more than 1024 processors, since the statically
/// sized `cpu_set_t` only has room for that many processors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuSet(Box<[u64]>);

impl Default for CpuSet {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSet {
    /// Allocates a new empty CPU set.
    pub fn new() -> Self {
        // Size the buffer according to the maximum number of processors.
        let size = (max_procs() + 63) as usize / 64;
        Self(vec![0; size].into())
    }

    /// Gets the length of the buffer in bytes, for use with syscalls.
    pub fn buffer_len(&self) -> usize {
        self.0.len() * 8
    }

    /// Gets a pointer for use with syscalls.
    pub fn as_ptr(&self) -> *const libc::cpu_set_t {
        self.0.as_ptr().cast()
    }

    /// Gets a mutable pointer for use with syscalls.
    pub fn as_mut_ptr(&mut self) -> *mut libc::cpu_set_t {
        self.0.as_mut_ptr().cast()
    }

    /// Sets processor `index` in the CPU set.
    ///
    /// Panics if `index` is greater than or equal to [`max_procs`].
    pub fn set(&mut self, index: u32) -> &mut Self {
        assert!(index < max_procs());
        // Can't use libc::CPU_SET because it assumes a statically sized
        // cpu_set_t (which raises the question of why they bother to expose
        // CPU_ALLOC_SIZE...).
        self.0[index as usize / 64] |= 1 << (index % 64);
        self
    }

    /// Sets all the CPUs in the linear bitmask `mask`, which is an ASCII
    /// hexadecimal string.
    ///
    /// This is useful for parsing the output of `/sys/devices/system/cpu/topology`.
    pub fn set_mask_hex_string(&mut self, string_mask: &[u8]) -> Result<(), InvalidHexString> {
        let err = || InvalidHexString(String::from_utf8_lossy(string_mask).into_owned());
        if !string_mask.len().is_multiple_of(2) {
            return Err(err());
        }
        let mask = string_mask
            .chunks_exact(2)
            .map(|s| u8::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok());
        for (i, byte) in mask.enumerate() {
            let byte = byte.ok_or_else(err)?;
            if byte != 0 {
                *self.0.get_mut(i / 8).ok_or_else(err)? |= (byte as u64) << (i % 8);
            }
        }
        Ok(())
    }

    /// Sets all the CPUs in the list `list`, which is a comma-separated list of
    /// ranges and single CPUs.
    ///
    /// This is useful for parsing the output of `/sys/devices/system/cpu/online`.
    pub fn set_mask_list(&mut self, list: &str) -> Result<(), InvalidCpuList> {
        let err = || InvalidCpuList(list.to_owned());
        for range in list.trim_end().split(',') {
            let range = match range.split_once('-') {
                Some((start, end)) => {
                    start.parse().map_err(|_| err())?..=end.parse().map_err(|_| err())?
                }
                None => {
                    let cpu = range.parse().map_err(|_| err())?;
                    cpu..=cpu
                }
            };
            for cpu in range {
                self.set(cpu);
            }
        }
        Ok(())
    }

    /// Returns whether processor `index` is set.
    ///
    /// Panics if `index` is greater than or equal to [`max_procs`].
    pub fn is_set(&self, index: u32) -> bool {
        assert!(index < max_procs());
        self.0[index as usize / 64] & (1 << (index % 64)) != 0
    }
}

#[derive(Debug, Error)]
#[error("invalid hex string for bitmask: {0}")]
pub struct InvalidHexString(String);

#[derive(Debug, Error)]
#[error("invalid CPU list: {0}")]
pub struct InvalidCpuList(String);

/// Sets the current thread's affinity.
pub fn set_current_thread_affinity(cpu_set: &CpuSet) -> io::Result<()> {
    // SAFETY: calling as documented, with an appropriately-sized buffer.
    let r = unsafe { libc::sched_setaffinity(0, cpu_set.buffer_len(), cpu_set.as_ptr()) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gets the current thread's affinity.
pub fn get_current_thread_affinity(cpu_set: &mut CpuSet) -> io::Result<()> {
    // SAFETY: calling as documented, with an appropriately-sized buffer.
    let r = unsafe { libc::sched_getaffinity(0, cpu_set.buffer_len(), cpu_set.as_mut_ptr()) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns the number of the processor the current thread was running on during the call to this function.
pub fn get_cpu_number() -> u32 {
    // SAFETY: Function has no preconditions.
    unsafe { libc::sched_getcpu() as u32 }
}

/// Returns the total number of online processors.
pub fn num_procs() -> u32 {
    static NUM_PROCS: OnceLock<u32> = OnceLock::new();
    *NUM_PROCS.get_or_init(|| {
        // Get the number of bits in the current affinity set. This isn't
        // perfect--what if we have set affinity to something else--but it's
        // what `sysconf(_SC_NPROCESSORS_ONLN)` does, except that it works with
        // more than 1024 processors.
        //
        // FUTURE: find callers of this and choose a different strategy
        // accordingly.
        let mut set = CpuSet::new();
        get_current_thread_affinity(&mut set).unwrap();
        set.0.iter().map(|x| x.count_ones()).sum()
    })
}

/// Returns the maximum CPU number of any present (but not necessarily online)
/// processor.
pub fn max_present_cpu() -> io::Result<u32> {
    let mut max_cpu = 0;
    for entry in fs_err::read_dir("/sys/devices/system/cpu")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(cpu) = name
            .to_str()
            .and_then(|s| s.strip_prefix("cpu"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        max_cpu = cpu.max(max_cpu);
    }
    Ok(max_cpu)
}

/// Returns the kernel compiled-in maximum number of processors.
pub fn max_procs() -> u32 {
    static MAX_PROCS: OnceLock<u32> = OnceLock::new();
    *MAX_PROCS.get_or_init(|| {
        let max_cpu_index: u32 = std::fs::read_to_string("/sys/devices/system/cpu/kernel_max")
            .expect("failed to read kernel_max")
            .trim_end()
            .parse()
            .expect("failed to parse kernel_max");

        max_cpu_index + 1
    })
}

#[cfg(test)]
mod tests {
    use super::max_procs;

    #[test]
    fn test_max_procs() {
        let p = max_procs();
        assert!(p > 0 && p < 32768);
    }

    #[test]
    fn test_cpu_list() {
        let mut set = super::CpuSet::new();
        set.set_mask_list("0-3,5").unwrap();
        assert_eq!(set.is_set(0), true);
        assert_eq!(set.is_set(1), true);
        assert_eq!(set.is_set(2), true);
        assert_eq!(set.is_set(3), true);
        assert_eq!(set.is_set(4), false);
        assert_eq!(set.is_set(5), true);
        assert_eq!(set.is_set(6), false);
    }
}
