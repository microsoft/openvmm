// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-platform helpers for process tree enumeration and memory measurement.

#[cfg(target_os = "linux")]
pub use self::linux::*;
#[cfg(not(any(target_os = "linux", windows)))]
pub use self::stub::*;
#[cfg(windows)]
pub use self::win_impl::*;

/// Memory statistics for a process tree.
pub struct TreeMemory {
    /// Total RSS across all processes in the tree, in KiB.
    pub rss_kib: u64,
    /// Private RSS (Private_Clean + Private_Dirty) in KiB.
    /// Excludes MAP_SHARED mappings such as guest RAM.
    /// Only available on Linux; on Windows this equals `rss_kib`.
    pub private_kib: u64,
    /// Total PSS (Proportional Set Size) in KiB. Only available on Linux.
    pub pss_kib: Option<u64>,
    /// Number of processes in the tree.
    pub process_count: u32,
}

/// A single large memory mapping from smaps.
pub struct SmapsMapping {
    /// Parsed virtual address range.
    pub addr_range: std::ops::Range<u64>,
    /// Permissions (e.g. "rw-p").
    pub perms: String,
    /// Mapping name/path (e.g. `\[heap\]`, "/usr/bin/openvmm", "").
    pub name: String,
    /// RSS in KiB.
    pub rss_kib: u64,
    /// Private (Clean+Dirty) in KiB.
    pub private_kib: u64,
}

/// A breakdown of memory by category.
pub struct SmapsBreakdown {
    /// Mappings with RSS > 0, sorted by private_kib descending.
    pub mappings: Vec<SmapsMapping>,
    /// RssFile from `/proc/{pid}/status` (file-backed pages).
    pub rss_file_kib: u64,
    /// RssAnon from `/proc/{pid}/status` (anonymous pages).
    pub rss_anon_kib: u64,
    /// RssShmem from `/proc/{pid}/status` (shared memory pages, e.g. guest RAM).
    pub rss_shmem_kib: u64,
    /// Private bytes in named guest RAM mappings (`[anon:guest-ram-*]`).
    /// Subtract from total private to get VMM-only overhead.
    pub guest_ram_private_kib: u64,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::Context as _;

    /// Get available host memory in bytes.
    pub fn available_memory_bytes() -> anyhow::Result<u64> {
        let contents =
            std::fs::read_to_string("/proc/meminfo").context("failed to read /proc/meminfo")?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let kib: u64 = rest
                    .trim()
                    .strip_suffix("kB")
                    .context("unexpected MemAvailable format")?
                    .trim()
                    .parse()
                    .context("failed to parse MemAvailable")?;
                return Ok(kib * 1024);
            }
        }
        anyhow::bail!("MemAvailable not found in /proc/meminfo")
    }

    /// Collect all PIDs in the process tree rooted at `root_pid`.
    ///
    /// Returns `root_pid` plus all descendant PIDs. If a process exits
    /// mid-walk, it is silently skipped.
    pub fn collect_process_tree(root_pid: i32) -> Vec<i32> {
        let mut result = vec![root_pid];
        let mut queue = vec![root_pid];

        while let Some(pid) = queue.pop() {
            // We must enumerate all threads, not just the main thread,
            // because fork()/clone() sets the child's parent to the
            // specific thread that called it. Each child appears in
            // exactly one thread's `children` file, so there are no
            // duplicates.
            let task_dir = format!("/proc/{pid}/task");
            let tids: Vec<i32> = match std::fs::read_dir(&task_dir) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
                    .collect(),
                Err(_) => continue, // process exited
            };

            for tid in tids {
                let children_path = format!("/proc/{pid}/task/{tid}/children");
                let children_str = match std::fs::read_to_string(&children_path) {
                    Ok(s) => s,
                    Err(_) => continue, // thread exited
                };
                for token in children_str.split_whitespace() {
                    if let Ok(child_pid) = token.parse::<i32>() {
                        result.push(child_pid);
                        queue.push(child_pid);
                    }
                }
            }
        }

        result
    }

    /// Measure memory for all processes in the given PID list.
    ///
    /// Processes that have exited are silently skipped.
    pub fn measure_tree_memory(pids: &[i32]) -> anyhow::Result<TreeMemory> {
        let mut total_rss: u64 = 0;
        let mut total_private: u64 = 0;
        let mut total_pss: u64 = 0;
        let mut count: u32 = 0;

        for &pid in pids {
            let path = format!("/proc/{pid}/smaps_rollup");
            let contents = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue, // process exited
            };

            let mut rss = 0u64;
            let mut pss = 0u64;
            let mut private_clean = 0u64;
            let mut private_dirty = 0u64;

            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("Rss:") {
                    if let Some(val) = parse_kb_value(rest) {
                        rss += val;
                    }
                } else if let Some(rest) = line.strip_prefix("Pss:") {
                    if let Some(val) = parse_kb_value(rest) {
                        pss += val;
                    }
                } else if let Some(rest) = line.strip_prefix("Private_Clean:") {
                    if let Some(val) = parse_kb_value(rest) {
                        private_clean += val;
                    }
                } else if let Some(rest) = line.strip_prefix("Private_Dirty:") {
                    if let Some(val) = parse_kb_value(rest) {
                        private_dirty += val;
                    }
                }
            }

            total_rss += rss;
            total_private += private_clean + private_dirty;
            total_pss += pss;
            count += 1;
        }

        Ok(TreeMemory {
            rss_kib: total_rss,
            private_kib: total_private,
            pss_kib: Some(total_pss),
            process_count: count,
        })
    }

    /// Parse a value like `"   12345 kB"` into `12345`.
    fn parse_kb_value(s: &str) -> Option<u64> {
        s.trim().strip_suffix("kB")?.trim().parse().ok()
    }

    /// Parse a smaps address range like "7f1234000000-7f1234100000" into a `Range<u64>`.
    fn parse_addr_range(range: &str) -> std::ops::Range<u64> {
        let Some((start_s, end_s)) = range.split_once('-') else {
            return 0..0;
        };
        let start = u64::from_str_radix(start_s, 16).unwrap_or(0);
        let end = u64::from_str_radix(end_s, 16).unwrap_or(0);
        start..end
    }

    /// Read detailed smaps breakdown for a process, including per-mapping RSS.
    ///
    /// Returns mappings sorted by private_kib descending plus RSS category
    /// totals from `/proc/{pid}/status`.
    ///
    /// `guest_mem_size` is the configured guest RAM in bytes. When VMA naming
    /// is unavailable (kernels without `CONFIG_ANON_VMA_NAME`), it is used as a
    /// heuristic to identify guest RAM mappings by their VA span.
    pub fn read_smaps_detail(pid: i32, guest_mem_size: u64) -> anyhow::Result<SmapsBreakdown> {
        // Read RSS category breakdown from /proc/{pid}/status.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .context("failed to read status")?;
        let mut rss_file_kib = 0u64;
        let mut rss_anon_kib = 0u64;
        let mut rss_shmem_kib = 0u64;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("RssFile:") {
                rss_file_kib = parse_kb_value(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("RssAnon:") {
                rss_anon_kib = parse_kb_value(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("RssShmem:") {
                rss_shmem_kib = parse_kb_value(rest).unwrap_or(0);
            }
        }

        // Parse /proc/{pid}/smaps for per-mapping detail.
        let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps"))
            .context("failed to read smaps")?;

        let mut mappings = Vec::new();
        let mut cur_range = String::new();
        let mut cur_perms = String::new();
        let mut cur_name = String::new();
        let mut cur_rss = 0u64;
        let mut cur_private = 0u64;

        let flush = |mappings: &mut Vec<SmapsMapping>,
                     range: &str,
                     perms: &str,
                     name: &str,
                     rss: u64,
                     private: u64| {
            if rss > 0 {
                mappings.push(SmapsMapping {
                    addr_range: parse_addr_range(range),
                    perms: perms.to_string(),
                    name: name.to_string(),
                    rss_kib: rss,
                    private_kib: private,
                });
            }
        };

        for line in smaps.lines() {
            // Mapping header lines start with a hex address.
            if line.starts_with(|c: char| c.is_ascii_hexdigit()) {
                flush(
                    &mut mappings,
                    &cur_range,
                    &cur_perms,
                    &cur_name,
                    cur_rss,
                    cur_private,
                );
                // Parse the smaps header. The format is:
                //   addr perms offset dev inode    pathname
                // with variable whitespace padding before pathname.
                let mut parts = line.split_whitespace();
                cur_range = parts.next().unwrap_or("").to_string();
                cur_perms = parts.next().unwrap_or("").to_string();
                // Skip offset, dev, inode.
                let _ = parts.next();
                let _ = parts.next();
                let _ = parts.next();
                // Collect remaining tokens as the pathname.
                cur_name = parts.collect::<Vec<_>>().join(" ");
                cur_rss = 0;
                cur_private = 0;
            } else if let Some(rest) = line.strip_prefix("Rss:") {
                cur_rss += parse_kb_value(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Private_Clean:") {
                cur_private += parse_kb_value(rest).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Private_Dirty:") {
                cur_private += parse_kb_value(rest).unwrap_or(0);
            }
        }
        flush(
            &mut mappings,
            &cur_range,
            &cur_perms,
            &cur_name,
            cur_rss,
            cur_private,
        );

        // Sort by private descending.
        mappings.sort_by_key(|a| std::cmp::Reverse(a.private_kib));

        // Sum private bytes from mappings named [anon:guest-ram-*].
        let mut guest_ram_private_kib: u64 = mappings
            .iter()
            .filter(|m| m.name.starts_with("[anon:guest-ram"))
            .map(|m| m.private_kib)
            .sum();

        // Fallback heuristic when VMA naming is unavailable (kernel lacks
        // CONFIG_ANON_VMA_NAME): identify guest RAM by looking for unnamed
        // anonymous rw-p mappings whose VA span covers the guest memory.
        if guest_ram_private_kib == 0 && guest_mem_size > 0 {
            guest_ram_private_kib = mappings
                .iter()
                .filter(|m| {
                    m.name.is_empty()
                        && m.perms.contains('w')
                        && m.perms.ends_with('p')
                        && m.addr_range.end - m.addr_range.start >= guest_mem_size
                })
                .map(|m| m.private_kib)
                .sum();
        }

        Ok(SmapsBreakdown {
            mappings,
            rss_file_kib,
            rss_anon_kib,
            rss_shmem_kib,
            guest_ram_private_kib,
        })
    }
}

#[cfg(target_os = "windows")]
mod win_impl {
    // UNSAFETY: FFI calls to Win32 API.
    #![expect(unsafe_code)]

    use super::*;
    use anyhow::Context as _;

    /// Get available host memory in bytes.
    pub fn available_memory_bytes() -> anyhow::Result<u64> {
        use windows::Win32::System::SystemInformation::GlobalMemoryStatusEx;
        use windows::Win32::System::SystemInformation::MEMORYSTATUSEX;

        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: Calling Win32 API with a properly-sized struct.
        unsafe {
            GlobalMemoryStatusEx(&mut status).context("GlobalMemoryStatusEx failed")?;
        }
        Ok(status.ullAvailPhys)
    }

    /// Collect all PIDs in the process tree rooted at `root_pid`.
    pub fn collect_process_tree(root_pid: i32) -> Vec<i32> {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
        use windows::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32;
        use windows::Win32::System::Diagnostics::ToolHelp::Process32First;
        use windows::Win32::System::Diagnostics::ToolHelp::Process32Next;
        use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;

        let root = root_pid as u32;

        // SAFETY: Taking a snapshot of the process list; no mutable state.
        let raw_snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            Ok(h) => h,
            Err(_) => return vec![root_pid],
        };
        // SAFETY: CreateToolhelp32Snapshot returns an owned handle.
        let _snapshot = unsafe { OwnedHandle::from_raw_handle(raw_snapshot.0.cast()) };

        // Build parent -> children map.
        let mut children_map: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        let mut entry = PROCESSENTRY32 {
            dwSize: size_of::<PROCESSENTRY32>() as u32,
            ..Default::default()
        };

        // SAFETY: entry.dwSize is set correctly; snapshot is valid.
        if unsafe { Process32First(raw_snapshot, &mut entry) }.is_ok() {
            loop {
                children_map
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push(entry.th32ProcessID);

                entry = PROCESSENTRY32 {
                    dwSize: size_of::<PROCESSENTRY32>() as u32,
                    ..Default::default()
                };
                // SAFETY: entry.dwSize is set correctly; snapshot is valid.
                if unsafe { Process32Next(raw_snapshot, &mut entry) }.is_err() {
                    break;
                }
            }
        }

        // Walk from root.
        let mut result = vec![root_pid];
        let mut queue = vec![root];
        while let Some(pid) = queue.pop() {
            if let Some(kids) = children_map.get(&pid) {
                for &kid in kids {
                    result.push(kid as i32);
                    queue.push(kid);
                }
            }
        }

        result
    }

    /// Read detailed smaps breakdown (Windows stub — not available).
    pub fn read_smaps_detail(_pid: i32, _guest_mem_size: u64) -> anyhow::Result<SmapsBreakdown> {
        anyhow::bail!("smaps detail not available on Windows")
    }

    /// Measure memory for all processes in the given PID list.
    pub fn measure_tree_memory(pids: &[i32]) -> anyhow::Result<TreeMemory> {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;
        use windows::Win32::System::ProcessStatus::GetProcessMemoryInfo;
        use windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;
        use windows::Win32::System::Threading::OpenProcess;
        use windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

        let mut total_rss: u64 = 0;
        let mut count: u32 = 0;

        for &pid in pids {
            let raw_handle =
            // SAFETY: Opening process handle with limited query rights.
            match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) } {
                Ok(h) => h,
                Err(_) => continue, // process exited or access denied
            };
            // SAFETY: OpenProcess returns an owned handle.
            let _handle = unsafe { OwnedHandle::from_raw_handle(raw_handle.0.cast()) };

            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            // SAFETY: handle is valid, counters.cb is set correctly.
            if unsafe {
                GetProcessMemoryInfo(
                    raw_handle,
                    &mut counters,
                    size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                )
            }
            .is_ok()
            {
                // WorkingSetSize is in bytes; convert to KiB.
                total_rss += counters.WorkingSetSize as u64 / 1024;
                count += 1;
            }
        }

        Ok(TreeMemory {
            rss_kib: total_rss,
            private_kib: total_rss,
            pss_kib: None,
            process_count: count,
        })
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod stub {
    use super::*;

    /// Get available host memory in bytes (unsupported on this platform).
    pub fn available_memory_bytes() -> anyhow::Result<u64> {
        anyhow::bail!("available_memory_bytes not implemented on this platform")
    }

    /// Collect all PIDs in the process tree (stub: returns only the root).
    pub fn collect_process_tree(root_pid: i32) -> Vec<i32> {
        vec![root_pid]
    }

    /// Measure memory for a process tree (unsupported on this platform).
    pub fn measure_tree_memory(_pids: &[i32]) -> anyhow::Result<TreeMemory> {
        anyhow::bail!("measure_tree_memory not implemented on this platform")
    }

    /// Read detailed smaps breakdown (unsupported on this platform).
    pub fn read_smaps_detail(_pid: i32, _guest_mem_size: u64) -> anyhow::Result<SmapsBreakdown> {
        anyhow::bail!("smaps detail not available on this platform")
    }
}
