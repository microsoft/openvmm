// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux host OS information from procfs and sysfs.

use super::HostOs;
use super::OsInfo;
use std::collections::BTreeSet;

const CLOCKSOURCE_DIR: &str = "/sys/devices/system/clocksource/clocksource0";

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

pub(super) fn os_info() -> OsInfo {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let (cpu_flags, microcode) = parse_cpuinfo(&cpuinfo);
    let mut available_clocksources =
        read_trimmed(&format!("{CLOCKSOURCE_DIR}/available_clocksource"))
            .map(|text| {
                text.split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
    available_clocksources.sort();
    OsInfo {
        os: Some(HostOs {
            kind: std::env::consts::OS.to_owned(),
            release: read_trimmed("/proc/sys/kernel/osrelease"),
            version: read_trimmed("/proc/sys/kernel/version"),
            cpu_flags,
            clocksource: read_trimmed(&format!("{CLOCKSOURCE_DIR}/current_clocksource")),
            available_clocksources,
        }),
        microcode,
    }
}

/// Returns the sorted CPU flags of the first processor in `/proc/cpuinfo`,
/// and the distinct microcode revisions of all processors.
fn parse_cpuinfo(cpuinfo: &str) -> (Vec<String>, Vec<String>) {
    let mut flags = None;
    let mut microcode = BTreeSet::new();
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "flags" if flags.is_none() => {
                flags = Some(
                    value
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<BTreeSet<_>>(),
                );
            }
            "microcode" => {
                microcode.insert(value.trim().to_ascii_lowercase());
            }
            _ => {}
        }
    }
    (
        flags.unwrap_or_default().into_iter().collect(),
        microcode.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_cpuinfo;
    use test_with_tracing::test;

    #[test]
    fn parses_flags_and_microcode() {
        let cpuinfo = "\
processor\t: 0
vendor_id\t: GenuineIntel
microcode\t: 0x2007006
flags\t\t: fpu tsc nonstop_tsc constant_tsc tsc
bugs\t\t: spectre_v1

processor\t: 1
microcode\t: 0x2007006
flags\t\t: fpu tsc
";
        let (flags, microcode) = parse_cpuinfo(cpuinfo);
        assert_eq!(flags, ["constant_tsc", "fpu", "nonstop_tsc", "tsc"]);
        assert_eq!(microcode, ["0x2007006"]);

        let (flags, microcode) = parse_cpuinfo("microcode : 0xFFFFFFFF\nmicrocode : 0x1\n");
        assert!(flags.is_empty());
        assert_eq!(microcode, ["0x1", "0xffffffff"]);
    }
}
