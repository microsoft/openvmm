// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Print information about the current system to the log

use flowey::node::prelude::*;

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

fn print_system_info(_rt: &mut RustRuntimeServices<'_>) {
    use sysinfo::{Disks, Networks, System};
    let sys = System::new_all();

    log::info!(
        "Memory: {:.1} GB / {:.1} GB",
        bytes_to_gibibytes(sys.used_memory()),
        bytes_to_gibibytes(sys.total_memory())
    );

    log::info!("CPUs: {}", sys.cpus().len());

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
}
