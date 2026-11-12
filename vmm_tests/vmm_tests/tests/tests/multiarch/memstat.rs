// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory Validation for VMM Tests

use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::IsolationType;
use petri::MemoryConfig;
use petri::OpenvmmLogConfig;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri_artifacts_common::tags::MachineArch;
use serde_json::Value;
use serde_json::from_str;
use std::time::Duration;
use vmm_test_macros::vmm_test;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq)]
pub enum TestVPCount {
    SmallVPCount = 2,
    LargeVPCount = 32,
}

#[repr(u64)]
pub enum WaitPeriodSec {
    ShortWait = 10,
    LongWait = 15,
}

fn get_arch_str(isolation_type: Option<IsolationType>, machine_arch: MachineArch) -> String {
    isolation_type
        .map(|isolation_type| match isolation_type {
            IsolationType::Vbs => "vbs-x64",
            IsolationType::Snp => "amd-snp",
            IsolationType::Tdx => "intel-tdx",
            IsolationType::Cca => "cca",
        })
        .unwrap_or_else(|| match machine_arch {
            MachineArch::Aarch64 => "aarch64",
            MachineArch::X86_64 => "gp-x64",
        })
        .to_string()
}

// evaluate validity based on a percent change from the baseline
fn is_under_threshold(base: f64, current: f64, threshold: f64) -> bool {
    ((current - base) / base) <= threshold
}

fn assert_against_baseline(proc_json: Value, build_flavor: &str, arch_str: &str, vp_string: &str) {
    let baseline_json: Value = from_str(include_str!("../../../test_data/memstat_baseline.json"))
        .expect(
            "the contents of memstat_baseline.json are expected to be parsable into a json object",
        );
    let vtl2_total = baseline_json[&arch_str]["vtl2_total"].as_f64().unwrap();

    let usage = vtl2_total
        - (proc_json["meminfo"]["ZoneInfoFreeMem"].as_f64().unwrap()
            + proc_json["meminfo"]["ZoneInfoPerCPUMem"].as_f64().unwrap());
    let usage_baseline = baseline_json[&arch_str][&vp_string]["usage"][&build_flavor]
        .as_f64()
        .unwrap();
    assert!(
        is_under_threshold(usage_baseline, usage, 0.25),
        ">25% regression in overall memory usage: {} (expected: {})",
        usage,
        usage_baseline
    );

    let reservation = vtl2_total - proc_json["meminfo"]["MemTotal"].as_f64().unwrap();
    let reservation_baseline = baseline_json[&arch_str][&vp_string]["reservation"][&build_flavor]
        .as_f64()
        .unwrap();
    assert!(
        is_under_threshold(reservation_baseline, reservation, 0.25),
        ">25% regression in kernel memory reservation: {} (expected: {})",
        reservation,
        reservation_baseline
    );

    let active_anon = proc_json["meminfo"]["Active(anon)"].as_f64().unwrap();
    let active_anon_baseline = baseline_json[&arch_str][&vp_string]["active_anon"][&build_flavor]
        .as_f64()
        .unwrap();
    assert!(
        is_under_threshold(active_anon_baseline, active_anon, 0.25),
        ">25% regression in active anonymous memory: {} (expected: {})",
        active_anon,
        active_anon_baseline
    );

    let inactive_anon = proc_json["meminfo"]["Inactive(anon)"].as_f64().unwrap();
    let inactive_anon_baseline = baseline_json[&arch_str][&vp_string]["inactive_anon"]
        [&build_flavor]
        .as_f64()
        .unwrap();
    assert!(
        is_under_threshold(inactive_anon_baseline, inactive_anon, 0.25),
        ">25% regression in inactive anonymous memory: {} (expected: {})",
        inactive_anon,
        inactive_anon_baseline
    );

    for underhill_process in ["underhill-init", "openvmm_hcl", "underhill-vm"] {
        let rss = proc_json["processes"][underhill_process.to_string()]["VmRSS"]
            .as_f64()
            .unwrap();
        let rss_baseline = baseline_json[&arch_str][&vp_string]
            [format!("{}_rss", underhill_process)][&build_flavor]
            .as_f64()
            .unwrap();

        assert!(
            is_under_threshold(rss_baseline, rss, 0.25),
            ">25% regression in {} rss: {} (expected: {})",
            underhill_process,
            rss,
            rss_baseline
        );
    }
}

async fn idle_test<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    vps: TestVPCount,
    wait_time_sec: WaitPeriodSec,
    driver: DefaultDriver,
    build_flavor: &str,
) -> anyhow::Result<()> {
    let isolation_type = config.isolation();
    let machine_arch = config.arch();
    let vp_count = vps as u32;

    let vm_boot_result = config
        .with_processor_topology({
            ProcessorTopology {
                vp_count,
                ..Default::default()
            }
        })
        .with_memory({
            MemoryConfig {
                startup_bytes: 16 * (1024 * 1024 * 1024),
                ..Default::default()
            }
        })
        .with_openhcl_log_levels(OpenvmmLogConfig::BuiltInDefault)
        .run()
        .await;

    // The VM is expected to fail to boot on the internal Intel pipeline only for the large VM size. We still want the AMD test to execute so
    // we will keep the test and gracefully exit in case of a failure. Any other type of boot failure should still produce an error
    if vm_boot_result.is_err()
        && machine_arch == MachineArch::X86_64
        && isolation_type.is_none()
        && vps == TestVPCount::LargeVPCount
    {
        tracing::warn!(
            "VM failed to start with the given topology, this is expected for the internal Intel runner only"
        );
        return Ok(());
    }

    let (mut vm, agent) = vm_boot_result?;
    vm.wait_for_vtl2_ready().await?;

    // Wait for the guest to be booted
    agent.ping().await?;

    // This wait is needed to let the idle VM fully instantiate its memory - provides more accurate memory usage results
    PolledTimer::new(&driver)
        .sleep(Duration::from_secs(wait_time_sec as u64))
        .await;

    let arch_str = get_arch_str(isolation_type, machine_arch);
    let vp_str = format!("{}vp", vp_count);
    let inspect_proc = vm.inspect_openhcl("proc", Some(2), None).await.unwrap();
    let mut proc_json: Value = from_str(&inspect_proc.json().to_string()).unwrap();

    proc_json["vm_info"] = serde_json::json!({
        "build_flavor": build_flavor,
        "arch": arch_str,
        "vps": vp_str,
    });

    tracing::info!("MEMSTAT_START:{}:MEMSTAT_END", proc_json);

    // block assertions on release builds for now so that we can collect baselines
    if build_flavor == "debug" {
        assert_against_baseline(proc_json, build_flavor, &arch_str, &vp_str);
    }

    agent.power_off().await?;
    vm.wait_for_teardown().await?;

    Ok(())
}

#[cfg(not(debug_assertions))]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn memory_validation_release_small<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    idle_test(
        config,
        TestVPCount::SmallVPCount,
        WaitPeriodSec::ShortWait,
        driver,
        "release",
    )
    .await
}

#[cfg(debug_assertions)]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn memory_validation_debug_small<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    idle_test(
        config,
        TestVPCount::SmallVPCount,
        WaitPeriodSec::ShortWait,
        driver,
        "debug",
    )
    .await
}

#[cfg(not(debug_assertions))]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn memory_validation_release_very_heavy<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    idle_test(
        config,
        TestVPCount::LargeVPCount,
        WaitPeriodSec::LongWait,
        driver,
        "release",
    )
    .await
}

#[cfg(debug_assertions)]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2025_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn memory_validation_debug_very_heavy<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    idle_test(
        config,
        TestVPCount::LargeVPCount,
        WaitPeriodSec::LongWait,
        driver,
        "debug",
    )
    .await
}
