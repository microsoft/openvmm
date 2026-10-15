// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run the standalone VMM.Perf runner.

use flowey::node::prelude::*;

pub fn all_profiles() -> Vec<String> {
    vec![
        "boot-time".into(),
        "fio:vmbus".into(),
        "fio:virtio".into(),
        "iperf3:vmbus".into(),
        "iperf3:virtio".into(),
    ]
}

pub fn default_profiles() -> Vec<String> {
    vec!["boot-time".into(), "fio".into(), "iperf3".into()]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmmPerfRunOutput {
    pub results_dir: PathBuf,
    pub success: bool,
    pub exit_code: Option<i32>,
}

flowey_request! {
    pub struct Request {
        pub runner: ReadVar<crate::build_vmm_perf::VmmPerfOutput>,
        pub openvmm: ReadVar<crate::build_openvmm::OpenvmmOutput>,
        pub firmware: ReadVar<PathBuf>,
        pub runtime_archive: ReadVar<PathBuf>,
        pub output_dir: ReadVar<PathBuf>,
        pub temp_dir: Option<ReadVar<PathBuf>>,
        pub profiles: Vec<String>,
        pub vm_sizes_json: Option<String>,
        pub parameters_json: Option<String>,
        /// Wait for host dependencies to be installed before running VMM.Perf.
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        pub output: WriteVar<VmmPerfRunOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            runner,
            openvmm,
            firmware,
            runtime_archive,
            output_dir,
            temp_dir,
            profiles,
            vm_sizes_json,
            parameters_json,
            pre_run_deps,
            output,
        } = request;

        ctx.emit_rust_step("run VMM.Perf", |ctx| {
            pre_run_deps.claim(ctx);
            let runner = runner.claim(ctx);
            let openvmm = openvmm.claim(ctx);
            let firmware = firmware.claim(ctx);
            let runtime_archive = runtime_archive.claim(ctx);
            let output_dir = output_dir.claim(ctx);
            let temp_dir = temp_dir.map(|temp_dir| temp_dir.claim(ctx));
            let output = output.claim(ctx);

            move |rt| {
                let runner = match rt.read(runner) {
                    crate::build_vmm_perf::VmmPerfOutput::LinuxBin { bin, .. } => bin,
                    crate::build_vmm_perf::VmmPerfOutput::WindowsBin { exe, .. } => exe,
                };
                let openvmm = match rt.read(openvmm) {
                    crate::build_openvmm::OpenvmmOutput::LinuxBin { bin, .. } => bin,
                    crate::build_openvmm::OpenvmmOutput::WindowsBin { exe, .. } => exe,
                };
                let firmware = rt.read(firmware);
                let runtime_archive = rt.read(runtime_archive);
                let output_dir = rt.read(output_dir).absolute()?;
                let temp_dir = temp_dir
                    .map(|temp_dir| rt.read(temp_dir).absolute())
                    .transpose()?;

                runner.make_executable()?;
                openvmm.make_executable()?;
                fs_err::create_dir_all(&output_dir)?;
                if let Some(temp_dir) = &temp_dir {
                    fs_err::create_dir_all(temp_dir)?;
                }

                let mut args = vec![
                    "--openvmm".to_string(),
                    openvmm.display().to_string(),
                    "--firmware".to_string(),
                    firmware.display().to_string(),
                    "--runtime-archive".to_string(),
                    runtime_archive.display().to_string(),
                    "--output-dir".to_string(),
                    output_dir.display().to_string(),
                ];
                if let Some(temp_dir) = &temp_dir {
                    args.push("--temp-dir".into());
                    args.push(temp_dir.display().to_string());
                }
                for profile in &profiles {
                    args.push("--profile".into());
                    args.push(profile.clone());
                }
                if let Some(vm_sizes_json) = &vm_sizes_json {
                    args.push("--vm-sizes-json".into());
                    args.push(vm_sizes_json.clone());
                }
                if let Some(parameters_json) = &parameters_json {
                    args.push("--parameters-json".into());
                    args.push(parameters_json.clone());
                }

                let process = flowey::shell_cmd!(rt, "{runner} {args...}")
                    .ignore_status()
                    .output()?;
                if !process.stdout.is_empty() {
                    log::info!("{}", String::from_utf8_lossy(&process.stdout));
                }
                if !process.stderr.is_empty() {
                    log::warn!("{}", String::from_utf8_lossy(&process.stderr));
                }

                rt.write(
                    output,
                    &VmmPerfRunOutput {
                        results_dir: output_dir,
                        success: process.status.success(),
                        exit_code: process.status.code(),
                    },
                );
                Ok(())
            }
        });

        Ok(())
    }
}
