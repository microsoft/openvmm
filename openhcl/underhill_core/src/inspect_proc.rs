// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use cvm_tracing::CVM_ALLOWED;
use inspect::Request;
use inspect::Response;
use inspect::SensitivityLevel;
use pal_async::timer::PolledTimer;
use std::time::Duration;
use vmcore::vm_task::VmTaskDriver;

/// Writes inspection results based on the contents of /proc/meminfo and /proc/zoneinfo
fn inspect_meminfo(req: Request<'_>) {
    const FIELDS: &[&str] = &[
        "MemTotal",
        "MemFree",
        "MemAvailable",
        "Buffers",
        "Cached",
        "SwapCached",
        "Active",
        "Inactive",
        "Active(anon)",
        "Inactive(anon)",
        "Active(file)",
        "Inactive(file)",
        "Unevictable",
        "Mlocked",
        "SwapTotal",
        "SwapFree",
        "Dirty",
        "Writeback",
        "AnonPages",
        "Mapped",
        "Shmem",
        "KReclaimable",
        "Slab",
        "SReclaimable",
        "SUnreclaim",
        "KernelStack",
        "PageTables",
        "SecPageTables",
        "NFS_Unstable",
        "Bounce",
        "WritebackTmp",
        "CommitLimit",
        "Committed_AS",
        "VmallocTotal",
        "VmallocUsed",
        "VmallocChunk",
        "Percpu",
        "AnonHugePages",
        "ShmemHugePages",
        "ShmemPmdMapped",
        "FileHugePages",
        "FilePmdMapped",
        "Balloon",
        "DirectMap4k",
        "DirectMap2M",
        "DirectMap1G",
    ];

    let mut resp = req.respond();
    fn inner(resp: &mut Response<'_>) -> anyhow::Result<()> {
        let contents = fs_err::read_to_string("/proc/meminfo")?;
        for line in contents.lines() {
            let (name, rest) = line.split_once(':').context("line had no :")?;
            if FIELDS.contains(&name) {
                // Remove the units for lines that have it, just report a number
                let value = rest
                    .split_ascii_whitespace()
                    .next()
                    .context("line had no value")?;
                let value = value.parse::<u64>().context("value failed to parse")?;
                resp.sensitivity_field(name, SensitivityLevel::Safe, value);
            }
        }

        let contents = fs_err::read_to_string("/proc/zoneinfo")?;
        let mut zoneinfo_nr_free_pages: u64 = 0;
        let mut zoneinfo_per_cpu_page_count: u64 = 0;
        for line in contents.lines() {
            let mut split_line = line.split_ascii_whitespace();
            let Some(field) = split_line.next() else {
                continue;
            };

            if matches!(field, "count:") {
                // Per-CPU free page counts are not counted in nr_free_pages or MemFree. In memory validation this is added to the memory from nr_free_pages to have a more accurate
                // measurement of usage when comparing versions. These values are kept separate in the actual inspect counters to have a better breakdown of the distribution of memory usage.
                zoneinfo_per_cpu_page_count += split_line
                    .next()
                    .context("line had no value")?
                    .parse::<u64>()
                    .context("value failed to parse")?;
            } else if matches!(field, "nr_free_pages") {
                zoneinfo_nr_free_pages += split_line
                    .next()
                    .context("line had no value")?
                    .parse::<u64>()
                    .context("value failed to parse")?
            }
        }
        resp.sensitivity_field(
            "ZoneInfoFreeMem",
            SensitivityLevel::Safe,
            zoneinfo_nr_free_pages * 4,
        );
        resp.sensitivity_field(
            "ZoneInfoPerCPUMem",
            SensitivityLevel::Safe,
            zoneinfo_per_cpu_page_count * 4,
        );

        Ok(())
    }

    if let Err(e) = inner(&mut resp) {
        resp.sensitivity_field("error", SensitivityLevel::Safe, inspect::AsDebug(e));
    }
}

/// Writes inspection results based on the contents of /proc/interrupts
fn inspect_interrupts(req: Request<'_>) {
    fn inner(req: Request<'_>) -> Option<()> {
        let contents = std::fs::read("/proc/interrupts").ok()?;
        let mut resp = req.respond();
        let mut lines = std::str::from_utf8(&contents).ok()?.split('\n');
        let cpu_count = lines.next()?.split_ascii_whitespace().count();
        for line in lines {
            let (irq, mut rest) = line.split_once(':')?;
            let irq = irq.trim_start();
            resp.sensitivity_field_mut(
                irq,
                SensitivityLevel::Safe,
                &mut inspect::adhoc_mut(|req| {
                    let mut resp = req.respond();
                    resp.sensitivity_field_mut(
                        "cpu",
                        SensitivityLevel::Safe,
                        &mut inspect::adhoc_mut(|req| {
                            let mut resp = req.respond();
                            for cpu in 0..cpu_count {
                                let Some((count, next)) = rest.trim_start().split_once(' ') else {
                                    return;
                                };
                                let Ok(count) = count.parse::<u64>() else {
                                    return;
                                };
                                // Skip empty counts to keep sizes down.
                                if count != 0 {
                                    resp.sensitivity_field(
                                        &cpu.to_string(),
                                        SensitivityLevel::Safe,
                                        inspect::AsCounter(count),
                                    );
                                }
                                rest = next;
                            }
                        }),
                    );
                    let rest = rest.trim_start();
                    let (name, actions) = rest.split_once("  ").unwrap_or((rest, ""));
                    resp.sensitivity_field("name", SensitivityLevel::Safe, name)
                        .sensitivity_field("actions", SensitivityLevel::Safe, actions.trim());
                    if let Ok(irq) = irq.parse::<u32>() {
                        resp.sensitivity_field(
                            "affinity",
                            SensitivityLevel::Safe,
                            std::fs::read_to_string(format!("/proc/irq/{irq}/smp_affinity_list"))
                                .ok()
                                .as_ref()
                                .map(|s| s.trim()),
                        );
                    }
                }),
            );
        }
        Some(())
    }

    inner(req);
}

/// Writes inspection results based on the contents of /proc/<pid>/status for userspace processes
fn inspect_userspace_procs(req: Request<'_>) {
    const KTHREADD_PID_STRING: &str = "2"; // String so we don't have to parse to check it
    const FIELDS: &[&str] = &[
        "VmSize", "VmPeak", "VmRSS", "VmHWM", "RssAnon", "RssFile", "RssShmem",
    ];
    let mut resp = req.respond();

    fn inner(resp: &mut Response<'_>) -> anyhow::Result<()> {
        'entries: for proc_entry in fs_err::read_dir("/proc")? {
            let proc_entry = proc_entry?;
            if !proc_entry.file_type()?.is_dir() {
                continue;
            }
            if proc_entry.file_name() == KTHREADD_PID_STRING {
                continue;
            }
            if !proc_entry
                .file_name()
                .as_encoded_bytes()
                .iter()
                .all(|b| b.is_ascii_digit())
            {
                continue;
            }

            let status = fs_err::read_to_string(proc_entry.path().join("status"))?;
            let mut proc_name = None;

            for line in status.lines() {
                let (name, rest) = line.split_once(':').context("line had no :")?;
                if name == "Name" {
                    proc_name = Some(rest.trim());
                }
                if name == "PPid" {
                    if rest.trim() == KTHREADD_PID_STRING {
                        continue 'entries;
                    }
                }
            }

            let proc_name = proc_name.context("no process name found for non-kernel process")?;
            resp.sensitivity_child(proc_name, SensitivityLevel::Safe, |req| {
                fn inner2(resp: &mut Response<'_>, status: &str) -> anyhow::Result<()> {
                    for line in status.lines() {
                        let (name, rest) = line.split_once(':').context("line had no :")?;
                        if FIELDS.contains(&name) {
                            // Remove the units for lines that have it, just report a number
                            let value = rest
                                .split_ascii_whitespace()
                                .next()
                                .context("line had no value")?;
                            let value = value.parse::<u64>().context("value failed to parse")?;
                            resp.sensitivity_field(name, SensitivityLevel::Safe, value);
                        }
                    }
                    Ok(())
                }

                let mut resp = req.respond();
                if let Err(e) = inner2(&mut resp, &status) {
                    resp.sensitivity_field("error", SensitivityLevel::Safe, inspect::AsDebug(e));
                }
            });
        }
        Ok(())
    }

    if let Err(e) = inner(&mut resp) {
        resp.sensitivity_field("error", SensitivityLevel::Safe, inspect::AsDebug(e));
    }
}

/// Used for on-demand inspect calls.
pub fn inspect_proc(req: Request<'_>) {
    req.respond()
        .sensitivity_child("meminfo", SensitivityLevel::Safe, inspect_meminfo)
        .sensitivity_child("interrupts", SensitivityLevel::Safe, inspect_interrupts)
        .sensitivity_child("processes", SensitivityLevel::Safe, inspect_userspace_procs);
}

/// Used for periodic automatic logging.
pub async fn periodic_telemetry_task(driver: VmTaskDriver) {
    let mut timer = PolledTimer::new(&driver);
    // Wait 15 minutes before initial logging to give the guest time to boot and begin doing work
    timer.sleep(Duration::from_secs(60 * 15)).await;

    loop {
        let mut inspection = inspect::inspect(
            "",
            inspect::adhoc_mut(|r| {
                r.respond()
                    .child("meminfo", inspect_meminfo)
                    .child("processes", inspect_userspace_procs);
            }),
        );
        inspection.resolve().await;
        let results = inspection.results();
        let json = results.json();
        // The below message needs to be valid JSON for ease of processing
        tracing::info!(CVM_ALLOWED, "{{\"periodic_memory_status\":{}}}", json);
        // Wait a day before logging again
        timer.sleep(Duration::from_secs(60 * 60 * 24)).await;
    }
}
