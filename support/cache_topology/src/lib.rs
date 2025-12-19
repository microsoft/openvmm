// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides ways to describe a machine's cache topology and to query it from
//! the current running machine.

use thiserror::Error;

/// A machine's cache topology.
#[derive(Debug)]
pub struct CacheTopology {
    /// A list of caches.
    pub caches: Vec<Cache>,
}

/// A memory cache.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cache {
    /// The cache level, 1 being closest to the CPU.
    pub level: u8,
    /// The cache type.
    pub cache_type: CacheType,
    /// The CPUs that share this cache.
    pub cpus: Vec<u32>,
    /// The cache size in bytes.
    pub size: u32,
    /// The cache associativity. /// If `None`, this cache is fully associative.
    pub associativity: Option<u32>,
    /// The cache line size in bytes.
    pub line_size: u32,
}

/// A cache type.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheType {
    /// A data cache.
    Data,
    /// An instruction cache.
    Instruction,
    /// A unified cache.
    Unified,
}

/// An error returned by [`CacheTopology::from_host`].
#[derive(Debug, Error)]
pub enum HostTopologyError {
    /// An error occurred while retrieving the cache topology.
    #[error("os error retrieving cache topology")]
    Os(#[source] std::io::Error),
}

impl CacheTopology {
    /// Returns the cache topology of the current machine.
    pub fn from_host() -> Result<Self, HostTopologyError> {
        let mut caches = Self::host_caches().map_err(HostTopologyError::Os)?;
        caches.sort();
        caches.dedup();
        Ok(Self { caches })
    }
}

#[cfg(windows)]
// UNSAFETY: needed to call Win32 functions to query cache topology
#[expect(unsafe_code)]
mod windows {
    use super::CacheTopology;
    use crate::Cache;
    use crate::CacheType;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::System::SystemInformation;

    impl CacheTopology {
        pub(crate) fn host_caches() -> std::io::Result<Vec<Cache>> {
            let mut len = 0;
            // SAFETY: passing a zero-length buffer as allowed by this routine.
            let r = unsafe {
                SystemInformation::GetLogicalProcessorInformationEx(
                    SystemInformation::RelationCache,
                    std::ptr::null_mut(),
                    &mut len,
                )
            };
            assert_eq!(r, 0);
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return Err(err);
            }
            let mut buf = vec![0u8; len as usize];
            // SAFETY: passing a buffer of the correct size as returned by the
            // previous call.
            let r = unsafe {
                SystemInformation::GetLogicalProcessorInformationEx(
                    SystemInformation::RelationCache,
                    buf.as_mut_ptr().cast(),
                    &mut len,
                )
            };
            if r == 0 {
                return Err(std::io::Error::last_os_error());
            }

            let mut caches = Vec::new();

            let mut buf = buf.as_slice();
            while !buf.is_empty() {
                // SAFETY: the remaining buffer is guaranteed to be large enough to hold
                // the structure.
                let info = unsafe {
                    &*buf
                        .as_ptr()
                        .cast::<SystemInformation::SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
                };

                assert_eq!(info.Relationship, SystemInformation::RelationCache);
                buf = &buf[info.Size as usize..];

                // SAFETY: this is a cache entry, as guaranteed by the previous
                // assertion.
                let cache = unsafe { &info.Anonymous.Cache };

                // SAFETY: the buffer is guaranteed by Win32 to be large enough
                // to hold the group masks.
                let groups = unsafe {
                    std::slice::from_raw_parts(
                        cache.Anonymous.GroupMasks.as_ptr(),
                        cache.GroupCount as usize,
                    )
                };

                let mut cpus = Vec::new();
                for group in groups {
                    for i in 0..usize::BITS {
                        if group.Mask & (1 << i) != 0 {
                            cpus.push(group.Group as u32 * usize::BITS + i);
                        }
                    }
                }

                caches.push(Cache {
                    cpus,
                    level: cache.Level,
                    cache_type: match cache.Type {
                        SystemInformation::CacheUnified => CacheType::Unified,
                        SystemInformation::CacheInstruction => CacheType::Instruction,
                        SystemInformation::CacheData => CacheType::Data,
                        _ => continue,
                    },
                    size: cache.CacheSize,
                    associativity: if cache.Associativity == !0 {
                        None
                    } else {
                        Some(cache.Associativity.into())
                    },
                    line_size: cache.LineSize.into(),
                });
            }

            Ok(caches)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Cache;
    use super::CacheTopology;

    impl CacheTopology {
        pub(crate) fn host_caches() -> std::io::Result<Vec<Cache>> {
            let mut caches = Vec::new();
            for cpu_entry in fs_err::read_dir("/sys/devices/system/cpu")? {
                let cpu_path = cpu_entry?.path();
                if cpu_path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("cpu")
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_none()
                {
                    continue;
                }
                for entry in fs_err::read_dir(cpu_path.join("cache"))? {
                    let entry = entry?;
                    let path = entry.path();
                    if !path
                        .file_name()
                        .unwrap()
                        .to_str()
                        .is_some_and(|s| s.starts_with("index"))
                    {
                        continue;
                    }

                    let associativity = fs_err::read_to_string(path.join("ways_of_associativity"))?
                        .trim_end()
                        .parse()
                        .unwrap();

                    let mut cpus = Vec::new();
                    for range in fs_err::read_to_string(path.join("shared_cpu_list"))?
                        .trim_end()
                        .split(',')
                    {
                        if let Some((start, end)) = range.split_once('-') {
                            cpus.extend(
                                start.parse::<u32>().unwrap()..=end.parse::<u32>().unwrap(),
                            );
                        } else {
                            cpus.push(range.parse().unwrap());
                        }
                    }

                    let line_size_result = fs_err::read_to_string(path.join("coherency_line_size"));
                    let line_size = match line_size_result {
                        Ok(s) => s.trim_end().parse::<u32>().unwrap(),
                        Err(e) => match e.kind() {
                            std::io::ErrorKind::NotFound => 64,
                            _ => return std::io::Result::Err(e),
                        },
                    };
                    caches.push(Cache {
                        cpus,
                        level: fs_err::read_to_string(path.join("level"))?
                            .trim_end()
                            .parse()
                            .unwrap(),
                        cache_type: match fs_err::read_to_string(path.join("type"))?.trim_end() {
                            "Data" => super::CacheType::Data,
                            "Instruction" => super::CacheType::Instruction,
                            "Unified" => super::CacheType::Unified,
                            _ => continue,
                        },
                        size: fs_err::read_to_string(path.join("size"))?
                            .strip_suffix("K\n")
                            .unwrap()
                            .parse::<u32>()
                            .unwrap()
                            * 1024,
                        associativity: if associativity == 0 {
                            None
                        } else {
                            Some(associativity)
                        },
                        line_size,
                    });
                }
            }
            Ok(caches)
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Cache;
    use super::CacheTopology;

    impl CacheTopology {
        pub(crate) fn host_caches() -> std::io::Result<Vec<Cache>> {
            // TODO
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_host_cache_topology() {
        let topology = super::CacheTopology::from_host().unwrap();
        assert!(!topology.caches.is_empty());
        println!("{topology:?}");
    }
}
