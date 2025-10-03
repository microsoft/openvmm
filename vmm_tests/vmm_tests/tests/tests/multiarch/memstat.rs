// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory Validation for VMM Tests

use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::IsolationType;
use petri::MemoryConfig;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ShutdownKind;
use petri_artifacts_common::tags::MachineArch;
use pipette_client::PipetteClient;
use pipette_client::cmd;
use serde::Serialize;
use serde_json::Value;
use serde_json::from_str;
use serde_json::to_string;
use std::collections::HashMap;
use std::ops::Index;
use std::ops::IndexMut;
use std::time::Duration;

#[repr(u32)]
pub enum TestVPCount {
    SmallVPCount,
    LargeVPCount,
}

#[repr(u64)]
pub enum WaitPeriodSec {
    ShortWait = 10,
    LongWait = 15,
}

/// PerProcessMemstat struct collects statistics from a single process relevant to memory validation
#[derive(Serialize, Clone, Default)]
struct PerProcessMemstat {
    /// HashMap generated from the contents of the /proc/{process ID}/smaps_rollup file for an OpenHCL process
    /// sample output from /proc/{process ID}/smaps_rollup:
    ///
    /// 55aa6c4b7000-7fffa7f9a000 ---p 00000000 00:00 0                          [rollup]
    /// Rss:               13300 kB
    /// Pss:                5707 kB
    /// Pss_Anon:           3608 kB
    pub smaps_rollup: HashMap<String, u64>,

    /// HashMap generated from the contents of the /proc/{process ID}/statm file for an OpenHCL process
    /// sample output from /proc/{process ID}/statm:
    ///
    /// 5480 3325 2423 11 0 756 0
    pub statm: HashMap<String, u64>,
}

/// MemStat struct collects all relevant memory usage data from VTL2 in a VM
#[derive(Serialize, Clone, Default)]
struct MemStat {
    /// meminfo is a HashMap generated from the contents of the /proc/meminfo file
    /// sample content of /proc/meminfo:
    ///
    /// MemTotal:       65820456 kB
    /// MemFree:        43453176 kB
    /// MemAvailable:   44322124 kB
    pub meminfo: HashMap<String, u64>,

    /// total_free_memory_per_zone is an integer calculated by aggregating the free memory from each CPU zone in the /proc/zoneinfo file
    /// sample content of /proc/zoneinfo:
    ///
    /// Node 0, zone      DMA
    ///   per-node stats
    ///     ...
    ///       nr_free_pages 5013074
    ///       nr_zone_inactive_anon 0
    ///     ...
    ///     cpu: 0
    ///               count: 10
    ///               high: 14
    pub total_free_memory_per_zone: u64,

    /// underhill_init corresponds to the memory usage statistics for the underhill-init process
    pub underhill_init: PerProcessMemstat,

    /// openvmm_hcl corresponds to the memory usage statistics for the openvmm_hcl process
    pub openvmm_hcl: PerProcessMemstat,

    /// underhill_vm corresponds to the memory usage statistics for the underhill-vm process
    pub underhill_vm: PerProcessMemstat,

    /// baseline data to compare test results against
    baseline_json: Value,
}

impl MemStat {
    /// Construction of a MemStat object takes the vtl2 Pipette agent to query OpenHCL for memory statistics for VTL2 as a whole and for VTL2's processes
    pub async fn new(vtl2_agent: &PipetteClient) -> Self {
        let sh = vtl2_agent.unix_shell();
        let meminfo = Self::parse_memfile(
            sh.read_file("/proc/meminfo")
                .await
                .expect("VTL2 should have meminfo file"),
            0, // meminfo data starts at the first line of the /proc/meminfo file
            0, // first column is the statistic (ie. MemFree)
            1, // second column is the value in kB
        );

        // total_free_memory_per_zone collects the free memory pages for each numa node and the number of free pages for each
        // CPU zone to get the total free memory pages. This value is multiplied by four to convert to kB
        let total_free_memory_per_zone = sh
            .read_file("/proc/zoneinfo")
            .await
            .expect("VTL2 should have zoneinfo file")
            .lines()
            .filter(|&line| line.contains("nr_free_pages") || line.contains("count:"))
            .map(|line| {
                line.split_whitespace()
                    .nth(1)
                    .expect("'nr_free_pages' and 'count:' lines are expected to have at least 2 words split by whitespace")
                    .parse::<u64>()
                    .expect("The word at position 1 on the filtered lines is expected to contain a number value")
            })
            .sum::<u64>()
            * 4;
        let mut per_process_data: HashMap<String, PerProcessMemstat> = HashMap::new();
        for (key, value) in Self::parse_memfile(
            cmd!(sh, "ps")
                .read()
                .await
                .expect("'ps' command is expected to succeed and produce output"),
            1, // Skipping the first row since it contains the ps output headers
            3, // process name is the fourth column (index 3) of ps output
            0, // process ID is teh first column (index 0) of ps output
        )
        .iter()
        .filter(|(key, _)| key.contains("underhill") || key.contains("openvmm"))
        {
            // process names may contain unecessary additional characters (ie. /bin/openvmm_hcl or {underhill-vm})
            // the following cleans these strings to be more consistent and readable
            let process_name = key
                .split('/')
                .next_back()
                .expect("process names are expected to be non-empty")
                .trim_matches(|c| c == '{' || c == '}')
                .replace("-", "_");
            per_process_data.insert(
                process_name.clone(),
                PerProcessMemstat {
                    smaps_rollup: Self::parse_memfile(
                        sh.read_file(&format!("/proc/{}/smaps_rollup", value))
                            .await
                            .unwrap_or_else(|_| {
                                panic!(
                                    "process {} is expected to have a 'smaps_rollup' file",
                                    process_name
                                )
                            }),
                        1, // smaps data starts after the first line
                        0, // the first column in smaps is the metric (ie. Pss_Anon)
                        1, // the second column is the corresponding value in kB
                    ),
                    statm: Self::parse_statm(
                        sh.read_file(&format!("/proc/{}/statm", value))
                            .await
                            .unwrap_or_else(|_| {
                                panic!(
                                    "process {} is expected to have a 'statm' file",
                                    process_name
                                )
                            }),
                    ),
                },
            );
        }

        let baseline_json = from_str(include_str!("../../../test_data/memstat_baseline.json")).expect("the contents of memstat_baseline.json are expected to be parsable into a json object");

        Self {
            meminfo,
            total_free_memory_per_zone,
            underhill_init: per_process_data
                .get("underhill_init")
                .expect("per_process_data should have underhill_init data if the process exists")
                .clone(),
            openvmm_hcl: per_process_data
                .get("openvmm_hcl")
                .expect("per_process_data should have openvmm_hcl data if the process exists")
                .clone(),
            underhill_vm: per_process_data
                .get("underhill_vm")
                .expect("per_process_data should have underhill_vm data if the process exists")
                .clone(),
            baseline_json,
        }
    }

    /// Compares current statistics against baseline
    pub fn compare_to_baseline(self, arch: &str, vps: &str) -> anyhow::Result<()> {
        let baseline_usage = Self::get_upper_limit_value(&self.baseline_json[arch][vps]["usage"]);
        let cur_usage = self.meminfo["MemTotal"] - self.total_free_memory_per_zone;
        assert!(
            baseline_usage >= cur_usage,
            "baseline usage is less than current usage: {} < {}",
            baseline_usage,
            cur_usage
        );

        for underhill_process in ["underhill_init", "openvmm_hcl", "underhill_vm"] {
            let baseline_pss = Self::get_upper_limit_value(
                &self.baseline_json[arch][vps][underhill_process]["Pss"],
            );
            let cur_pss = self[underhill_process].smaps_rollup["Pss"];

            let baseline_pss_anon = Self::get_upper_limit_value(
                &self.baseline_json[arch][vps][underhill_process]["Pss_Anon"],
            );
            let cur_pss_anon = self[underhill_process].smaps_rollup["Pss_Anon"];

            assert!(
                baseline_pss >= cur_pss,
                "[process {}]: baseline PSS is less than current PSS: {} < {}",
                underhill_process,
                baseline_pss,
                cur_pss
            );
            assert!(
                baseline_pss_anon >= cur_pss_anon,
                "[process {}]: baseline PSS Anon is less than current PSS Anon: {} < {}",
                underhill_process,
                baseline_pss_anon,
                cur_pss_anon
            );
        }

        let baseline_reservation =
            Self::get_upper_limit_value(&self.baseline_json[arch][vps]["reservation"]);
        let cur_reservation =
            self.baseline_json[arch]["vtl2_total"].as_u64().unwrap() - self.meminfo["MemTotal"];
        assert!(
            baseline_reservation >= cur_reservation,
            "baseline reservation is less than current reservation: {} < {}",
            baseline_reservation,
            cur_reservation
        );

        Ok(())
    }

    fn parse_memfile(
        input: String,
        start_row: usize,
        field_col: usize,
        value_col: usize,
    ) -> HashMap<String, u64> {
        let mut parsed_data: HashMap<String, u64> = HashMap::new();
        for line in input.lines().skip(start_row) {
            let split_line = line.split_whitespace().collect::<Vec<&str>>();
            let field = split_line
                .get(field_col)
                .unwrap_or_else(|| panic!("in line {} column {} does not exist", line, field_col))
                .trim_matches(':')
                .to_string();
            let value: u64 = split_line
                .get(value_col)
                .unwrap_or_else(|| panic!("in line {} column {} does not exist", line, value_col))
                .parse::<u64>()
                .unwrap_or_else(|_| {
                    panic!(
                        "value column {} in line {} is expected to be a parsable u64 integer",
                        value_col, line
                    )
                });
            parsed_data.insert(field, value);
        }
        parsed_data
    }

    fn parse_statm(raw_statm_data: String) -> HashMap<String, u64> {
        // statm output consists of seven numbers split by spaces (ie. 5480 3325 ...) representing the following fields (in order):
        let statm_fields = [
            "vm_size",
            "vm_rss",
            "vm_shared",
            "text",
            "lib",
            "data",
            "dirty_pages",
        ];
        raw_statm_data
            .split_whitespace()
            .enumerate()
            .map(|(index, value)| {
                (
                    statm_fields
                        .get(index)
                        .unwrap_or_else(|| {
                            panic!(
                                "statm file is expected to contain at most {} items",
                                statm_fields.len()
                            )
                        })
                        .to_owned()
                        .to_string(),
                    value
                        .parse::<u64>()
                        .expect("all items in statm file are expected to be parsable u64 integers"),
                )
            })
            .collect::<HashMap<String, u64>>()
    }

    fn get_upper_limit_value(baseline_metric_json: &Value) -> u64 {
        const PANIC_MSG: &str =
            "all values in the memstat_baseline.json file are expected to be parsable u64 integers";

        baseline_metric_json["base"]
            .as_u64()
            .unwrap_or_else(|| panic!("{}", PANIC_MSG))
            + baseline_metric_json["threshold"]
                .as_u64()
                .unwrap_or_else(|| panic!("{}", PANIC_MSG))
    }
}

impl Index<&'_ str> for MemStat {
    type Output = PerProcessMemstat;
    fn index(&self, s: &str) -> &PerProcessMemstat {
        match s {
            "underhill_init" => &self.underhill_init,
            "openvmm_hcl" => &self.openvmm_hcl,
            "underhill_vm" => &self.underhill_vm,
            _ => panic!("memstat field {} does not exist or is not indexible", s),
        }
    }
}

impl IndexMut<&'_ str> for MemStat {
    fn index_mut(&mut self, s: &str) -> &mut PerProcessMemstat {
        match s {
            "underhill_init" => &mut self.underhill_init,
            "openvmm_hcl" => &mut self.openvmm_hcl,
            "underhill_vm" => &mut self.underhill_vm,
            _ => panic!("memstat field {} does not exist or is not indexible", s),
        }
    }
}

fn get_arch_str(isolation_type: Option<IsolationType>, machine_arch: MachineArch) -> String {
    isolation_type
        .map(|isolation_type| match isolation_type {
            IsolationType::Vbs => "vbs-x64",
            IsolationType::Snp => "amd-snp",
            IsolationType::Tdx => "intel-tdx",
        })
        .unwrap_or_else(|| match machine_arch {
            MachineArch::Aarch64 => "aarch64",
            MachineArch::X86_64 => "gp-x64",
        })
        .to_string()
}

pub async fn idle_test<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    vps: TestVPCount,
    wait_time_sec: WaitPeriodSec,
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    let isolation_type = config.isolation();
    let machine_arch = config.arch();
    let arch_str = get_arch_str(isolation_type, machine_arch);
    let vp_count = match vps {
        TestVPCount::SmallVPCount => 2,
        TestVPCount::LargeVPCount => {
            if arch_str.contains("x64") {
                32
            } else {
                64
            }
        }
    };
    let mut vm = config
        .with_processor_topology({
            ProcessorTopology {
                vp_count,
                ..Default::default()
            }
        })
        .with_memory({
            MemoryConfig {
                startup_bytes: 16 * (1024 * 1024 * 1024),
                dynamic_memory_range: None,
            }
        })
        .run_without_agent()
        .await?;
    let vtl2_agent = vm.wait_for_vtl2_agent().await?;

    // This wait is needed to let the idle VM fully instantiate its memory - provides more accurate memory usage results
    PolledTimer::new(&driver)
        .sleep(Duration::from_secs(wait_time_sec as u64))
        .await;

    let memstat = MemStat::new(&vtl2_agent).await;
    tracing::info!("MEMSTAT_START:{}:MEMSTAT_END", to_string(&memstat).unwrap());
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_teardown().await?;
    memstat.compare_to_baseline(&arch_str, &format!("{}vp", vp_count))?;

    Ok(())
}
