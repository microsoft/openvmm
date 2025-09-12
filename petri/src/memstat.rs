// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory Validation Data Collection for Petri Tests

use pipette_client::PipetteClient;
use pipette_client::cmd;
use serde::Serialize;
use std::collections::HashMap;

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
}

impl MemStat {
    /// Construction of a MemStat object takes the vtl2 Pipette agent to query OpenHCL for memory statistics for VTL2 as a whole and for VTL2's processes
    pub async fn new(vtl2_agent: &PipetteClient) -> Self {
        let sh = vtl2_agent.unix_shell();
        let meminfo = Self::parse_memfile(sh.read_file("/proc/meminfo").await.unwrap(), 0, 0, 1);
        let total_free_memory_per_zone = sh
            .read_file("/proc/zoneinfo")
            .await
            .unwrap()
            .lines()
            .filter(|&line| line.contains("nr_free_pages") || line.contains("count:"))
            .map(|line| {
                line.split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .sum::<u64>()
            * 4;
        let mut per_process_data: HashMap<String, PerProcessMemstat> = HashMap::new();
        for (key, value) in Self::parse_memfile(cmd!(sh, "ps").read().await.unwrap(), 1, 3, 0)
            .iter()
            .filter(|(key, _)| key.contains("underhill") || key.contains("openvmm"))
        {
            let process_name = key
                .split('/')
                .next_back()
                .unwrap()
                .trim_matches(|c| c == '{' || c == '}')
                .replace("-", "_");
            per_process_data.insert(
                process_name.clone(),
                PerProcessMemstat {
                    smaps_rollup: Self::parse_memfile(
                        sh.read_file(&format!("/proc/{}/smaps_rollup", value))
                            .await
                            .unwrap(),
                        1,
                        0,
                        1,
                    ),
                    statm: Self::parse_statm(
                        sh.read_file(&format!("/proc/{}/statm", value))
                            .await
                            .unwrap(),
                    ),
                },
            );
        }

        Self {
            meminfo,
            total_free_memory_per_zone,
            underhill_init: per_process_data.get("underhill_init").unwrap().clone(),
            openvmm_hcl: per_process_data.get("openvmm_hcl").unwrap().clone(),
            underhill_vm: per_process_data.get("underhill_vm").unwrap().clone(),
        }
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
                .unwrap()
                .trim_matches(':')
                .to_string();
            let value: u64 = split_line.get(value_col).unwrap_or(&"0").parse().unwrap();
            parsed_data.insert(field, value);
        }
        parsed_data
    }

    fn parse_statm(raw: String) -> HashMap<String, u64> {
        let mut statm: HashMap<String, u64> = HashMap::new();
        let split_arr = raw.split_whitespace().collect::<Vec<&str>>();
        statm.insert("vm_size".to_string(), split_arr[0].parse::<u64>().unwrap());
        statm.insert("vm_rss".to_string(), split_arr[1].parse::<u64>().unwrap());
        statm.insert(
            "vm_shared".to_string(),
            split_arr[2].parse::<u64>().unwrap(),
        );
        statm.insert("text".to_string(), split_arr[3].parse::<u64>().unwrap());
        statm.insert("lib".to_string(), split_arr[4].parse::<u64>().unwrap());
        statm.insert("data".to_string(), split_arr[5].parse::<u64>().unwrap());
        statm.insert(
            "dirty_pages".to_string(),
            split_arr[6].parse::<u64>().unwrap(),
        );
        statm
    }
}
