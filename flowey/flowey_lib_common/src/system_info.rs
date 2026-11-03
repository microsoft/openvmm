// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Print information about the current system to the log

use flowey::node::prelude::*;
use std::collections::BTreeMap;

new_simple_flow_node!(struct Node);

flowey_request! {
    pub struct Request {
        pub done: WriteVar<SideEffect>,
    }
}

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_dep: &mut ImportCtx<'_>) {
        // no deps
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        ctx.emit_rust_step("print system info", |ctx| {
            request.done.claim(ctx);
            |rt| {
                print_system_info(rt);
                Ok(())
            }
        });

        Ok(())
    }
}

fn bytes_to_gibibytes(bytes: u64) -> f64 {
    bytes as f64 / (1024 * 1024 * 1024) as f64
}

fn print_system_info(rt: &mut RustRuntimeServices<'_>) {
    use sysinfo::{Disks, Networks, System};
    let sys = System::new_all();

    log::info!(
        "Memory: {:.1} GB / {:.1} GB",
        bytes_to_gibibytes(sys.used_memory()),
        bytes_to_gibibytes(sys.total_memory())
    );

    let cpu_list = sys
        .cpus()
        .iter()
        .map(|cpu| (cpu.vendor_id(), cpu.brand(), cpu.frequency()))
        .collect::<Vec<_>>();

    let mut cpus = BTreeMap::new();
    for key in cpu_list {
        let count = cpus.entry(key).or_insert(0u64);
        *count += 1;
    }

    for ((vendor, brand, freq), count) in cpus {
        log::info!("CPU: {vendor} [{brand}] @ {freq} MHz × {count}");
    }

    let os_info = [
        System::name(),
        System::os_version(),
        System::kernel_version(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    log::info!("OS: {}", os_info);
    log::info!("Hostname: {}", System::host_name().unwrap_or_default());

    match azure_vm_id(rt) {
        Ok(vm_id) => log::info!("Azure VM ID: {vm_id:?}"),
        Err(err) => log::info!("Azure VM ID unavailable: {err:#}"),
    }

    let disks = Disks::new_with_refreshed_list();
    for disk in &disks {
        let used_space = disk.total_space() - disk.available_space();
        log::info!(
            "Disk: {} {:.1} GB / {:.1} GB",
            disk.mount_point().display(),
            bytes_to_gibibytes(used_space),
            bytes_to_gibibytes(disk.total_space())
        );
    }

    let networks = Networks::new_with_refreshed_list();
    for (interface_name, data) in &networks {
        let ip_addresses = data
            .ip_networks()
            .iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        log::info!("Network: {interface_name} {ip_addresses}");
    }

    let is_uefi = match rt.platform() {
        FlowPlatform::Windows => std::process::Command::new("bcdedit")
            .output()
            .ok()
            .and_then(|o| o.status.success().then(|| String::from_utf8(o.stdout).ok()))
            .flatten()
            .map(|o| o.to_lowercase().contains(".efi")),
        FlowPlatform::Linux(_) => Path::new("/sys/firmware/efi").try_exists().ok(),
        _ => None,
    };

    match is_uefi {
        Some(true) => log::info!("Using UEFI firmware"),
        Some(false) => log::info!("Not using UEFI firmware"),
        None => log::info!("Unknown firmware"),
    }
}

fn azure_vm_id(rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<String> {
    let output = flowey::shell_cmd!(rt, "curl")
        .args([
            "--disable",
            "--silent",
            "--show-error",
            "--fail",
            "--noproxy",
            "*",
            "--connect-timeout",
            "1",
            "--max-time",
            "2",
            "--header",
            "Metadata:true",
            "http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01",
        ])
        .quiet()
        .ignore_status()
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "IMDS request failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_azure_vm_id(&output.stdout)
}

fn parse_azure_vm_id(json: &[u8]) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct ComputeMetadata {
        #[serde(rename = "vmId")]
        vm_id: String,
    }

    let metadata: ComputeMetadata =
        serde_json::from_slice(json).context("invalid IMDS compute metadata")?;
    let vm_id = metadata.vm_id.trim();
    anyhow::ensure!(!vm_id.is_empty(), "IMDS compute metadata has no VM ID");
    Ok(vm_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_azure_vm_id;
    use test_with_tracing::test;

    #[test]
    fn azure_vm_id_from_compute_metadata() {
        let json = br#"{
            "vmId": " c6d5526c-5ac2-4200-b6e2-56f2b70c5ab2 ",
            "name": "different-id",
            "resourceId": "/subscriptions/example/virtualMachines/example"
        }"#;
        assert_eq!(
            parse_azure_vm_id(json).unwrap(),
            "c6d5526c-5ac2-4200-b6e2-56f2b70c5ab2"
        );
    }

    #[test]
    fn azure_vm_id_missing_or_empty() {
        for json in [r#"{}"#, r#"{"vmId": ""}"#, r#"{"vmId": " "}"#] {
            assert!(parse_azure_vm_id(json.as_bytes()).is_err());
        }
    }

    #[test]
    fn azure_vm_id_invalid_metadata() {
        for json in [r#"{"vmId":"#, r#"{"vmId": null}"#, r#"{"vmId": 123}"#] {
            assert!(parse_azure_vm_id(json.as_bytes()).is_err());
        }
    }
}
