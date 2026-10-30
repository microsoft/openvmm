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

    if matches!(
        rt.platform(),
        FlowPlatform::Linux(_) | FlowPlatform::Windows
    ) {
        match azure_container_id(rt) {
            Ok(container_id) => log::info!("Azure container ID: {container_id:?}"),
            Err(err) => log::info!("Azure container ID unavailable: {err:#}"),
        }
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

fn azure_container_id(rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<String> {
    let output = flowey::shell_cmd!(
        rt,
        "curl --disable --silent --show-error --fail --noproxy '*' --connect-timeout 1 --max-time 2 --header x-ms-version:2012-11-30 'http://168.63.129.16/machine?comp=goalstate'"
    )
    .quiet()
    .ignore_status()
    .output()?;
    anyhow::ensure!(
        output.status.success(),
        "WireServer request failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let xml = String::from_utf8(output.stdout).context("WireServer response is not UTF-8")?;
    parse_azure_container_id(&xml)
}

fn parse_azure_container_id(xml: &str) -> anyhow::Result<String> {
    let doc = roxmltree::Document::parse(xml).context("invalid WireServer goal state")?;
    let container_id = doc
        .root_element()
        .children()
        .find(|node| node.has_tag_name("Container"))
        .and_then(|node| {
            node.children()
                .find(|node| node.has_tag_name("ContainerId"))
        })
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .context("WireServer goal state has no container ID")?;
    Ok(container_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_azure_container_id;
    use test_with_tracing::test;

    #[test]
    fn azure_container_id_from_goal_state() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <GoalState>
                <Container>
                    <ContainerId> c6d5526c-5ac2-4200-b6e2-56f2b70c5ab2 </ContainerId>
                    <RoleInstanceList>
                        <RoleInstance><InstanceId>different-id</InstanceId></RoleInstance>
                    </RoleInstanceList>
                </Container>
            </GoalState>"#;
        assert_eq!(
            parse_azure_container_id(xml).unwrap(),
            "c6d5526c-5ac2-4200-b6e2-56f2b70c5ab2"
        );
    }

    #[test]
    fn azure_container_id_missing_or_empty() {
        for xml in [
            "<GoalState/>",
            "<GoalState><Container/></GoalState>",
            "<GoalState><Container><ContainerId> </ContainerId></Container></GoalState>",
        ] {
            assert!(parse_azure_container_id(xml).is_err());
        }
    }

    #[test]
    fn azure_container_id_malformed_xml() {
        assert!(parse_azure_container_id("<GoalState>").is_err());
    }
}
