// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory Validation Data Collection for Petri Tests

use pipette_client::PipetteClient;
use pipette_client::cmd;
use serde::Serialize;
use serde_json::Value;
use serde_json::from_reader;
use std::collections::HashMap;
use std::env::current_dir;
use std::fs::File;
use std::ops::Index;
use std::ops::IndexMut;
use std::path::Path;

/// PerProcessMemstat struct collects statistics from a single process relevant to memory validation
#[derive(Serialize, Clone, Default)]
pub struct PerProcessMemstat {
    /// HashMap generated from the contents of the /proc/{process ID}/smaps_rollup file for an OpenHCL process
    pub smaps_rollup: HashMap<String, u64>,

    /// HashMap generated from the contents of the /proc/{process ID}/statm file for an OpenHCL process
    pub statm: HashMap<String, u64>,
}

/// MemStat struct collects all relevant memory usage data from VTL2 in a VM
#[derive(Serialize, Clone, Default)]
pub struct MemStat {
    /// meminfo is a HashMap generated from the contents of the /proc/meminfo file
    pub meminfo: HashMap<String, u64>,

    /// total_free_memory_per_zone is an integer calculated by aggregating the free memory from each CPU zone in the /proc/zoneinfo file
    pub total_free_memory_per_zone: u64,

    /// underhill_init corresponds to the memory usage statistics for the underhill-init process
    pub underhill_init: PerProcessMemstat,

    /// openvmm_hcl corresponds to the memory usage statistics for the openvmm_hcl process
    pub openvmm_hcl: PerProcessMemstat,

    /// underhill_vm corresponds to the memory usage statistics for the underhill-vm process
    pub underhill_vm: PerProcessMemstat,

    /// json object to hold baseline values that test results are compared against
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
            0,
            0,
            1,
        );
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
            1,
            3,
            0,
        )
        .iter()
        .filter(|(key, _)| key.contains("underhill") || key.contains("openvmm"))
        {
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
                        1,
                        0,
                        1,
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

        let path_str = format!(
            "{}/test_data/memstat_baseline.json",
            current_dir()
                .expect("current_dir is expected to return a path string")
                .to_str()
                .unwrap()
        );
        let baseline_json = from_reader::<File, Value>(
            File::open(Path::new(&path_str))
                .unwrap_or_else(|_| panic!("{} file not found", path_str)),
        )
        .unwrap_or_else(|_| {
            panic!(
                "memstat json is expected to exist within the file {}",
                path_str
            )
        });

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
    pub fn compare_to_baseline(self, arch: &str, vps: &str) -> bool {
        let baseline_usage = Self::get_baseline_value(&self.baseline_json[arch][vps]["usage"]);
        let cur_usage = self.meminfo["MemTotal"] - self.total_free_memory_per_zone;
        assert!(
            baseline_usage >= cur_usage,
            "baseline usage is less than current usage: {} < {}",
            baseline_usage,
            cur_usage
        );

        let baseline_reservation =
            Self::get_baseline_value(&self.baseline_json[arch][vps]["reservation"]);
        let cur_reservation =
            self.baseline_json[arch]["vtl2_total"].as_u64().unwrap() - self.meminfo["MemTotal"];
        assert!(
            baseline_reservation >= cur_reservation,
            "baseline reservation is less than current reservation: {} < {}",
            baseline_reservation,
            cur_reservation
        );

        for prs in ["underhill_init", "openvmm_hcl", "underhill_vm"] {
            let baseline_pss = Self::get_baseline_value(&self.baseline_json[arch][vps][prs]["Pss"]);
            let cur_pss = self[prs].smaps_rollup["Pss"];

            let baseline_pss_anon =
                Self::get_baseline_value(&self.baseline_json[arch][vps][prs]["Pss_Anon"]);
            let cur_pss_anon = self[prs].smaps_rollup["Pss_Anon"];

            assert!(
                baseline_pss >= cur_pss,
                "[process {}]: baseline PSS is less than current PSS: {} < {}",
                prs,
                baseline_pss,
                cur_pss
            );
            assert!(
                baseline_pss_anon >= cur_pss_anon,
                "[process {}]: baseline PSS Anon is less than current PSS Anon: {} < {}",
                prs,
                baseline_pss_anon,
                cur_pss_anon
            );
        }

        true
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
                        "value column {} in line {} is expected to be a parsable u64",
                        value_col, line
                    )
                });
            parsed_data.insert(field, value);
        }
        parsed_data
    }

    fn parse_statm(raw: String) -> HashMap<String, u64> {
        let statm_fields = [
            "vm_size",
            "vm_rss",
            "vm_shared",
            "text",
            "lib",
            "data",
            "dirty_pages",
        ];
        raw.split_whitespace()
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
                        .expect("all items in statm file are expected to be parsable u64 numbers"),
                )
            })
            .collect::<HashMap<String, u64>>()
    }

    fn get_baseline_value(baseline_json: &Value) -> u64 {
        baseline_json["baseline"].as_u64().unwrap_or_else(|| {
            panic!("all values in the memstat_baseline.json file are expected to be parsable u64 numbers")
        }) +
            baseline_json["threshold"].as_u64().unwrap_or_else(|| {
                panic!("all values in the memstat_baseline.json file are expected to be parsable u64 numbers")
            })
    }
}

impl Index<&'_ str> for MemStat {
    type Output = PerProcessMemstat;
    fn index(&self, s: &str) -> &PerProcessMemstat {
        match s {
            "underhill_init" => &self.underhill_init,
            "openvmm_hcl" => &self.openvmm_hcl,
            "underhill_vm" => &self.underhill_vm,
            _ => panic!("unknown field: {}", s),
        }
    }
}

impl IndexMut<&'_ str> for MemStat {
    fn index_mut(&mut self, s: &str) -> &mut PerProcessMemstat {
        match s {
            "underhill_init" => &mut self.underhill_init,
            "openvmm_hcl" => &mut self.openvmm_hcl,
            "underhill_vm" => &mut self.underhill_vm,
            _ => panic!("unknown field: {}", s),
        }
    }
}
