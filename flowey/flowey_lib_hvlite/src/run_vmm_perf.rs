// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run the standalone VMM.Perf runner.

use flowey::node::prelude::*;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum VmmPerfProfile {
    BootTime,
    Fio,
    Iperf3,
}

impl VmmPerfProfile {
    pub fn all() -> Vec<Self> {
        vec![Self::BootTime, Self::Fio, Self::Iperf3]
    }

    fn cli_name(self) -> &'static str {
        match self {
            Self::BootTime => "boot-time",
            Self::Fio => "fio",
            Self::Iperf3 => "iperf3",
        }
    }
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
        pub profiles: Vec<VmmPerfProfile>,
        pub vm_sizes_json: Option<String>,
        pub parameters_json: Option<String>,
        /// Configure this 2 MiB hugetlb surplus-page overcommit limit before running.
        pub hugetlb_2mb_overcommit_pages: Option<u64>,
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
            hugetlb_2mb_overcommit_pages,
            output,
        } = request;

        ctx.emit_rust_step("run VMM.Perf", |ctx| {
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

                if !matches!(rt.backend(), FlowBackend::Local)
                    && matches!(rt.platform(), FlowPlatform::Linux(_))
                {
                    for device in ["/dev/kvm", "/dev/mshv"] {
                        if Path::new(device).exists() {
                            flowey::shell_cmd!(rt, "sudo chmod a+rw {device}").run()?;
                        }
                    }

                    if let Some(overcommit_pages) = hugetlb_2mb_overcommit_pages {
                        let hugepages_dir =
                            Path::new("/sys/kernel/mm/hugepages/hugepages-2048kB");
                        let write_overcommit_script = format!(
                            "echo {overcommit_pages} | sudo tee {}/nr_overcommit_hugepages >/dev/null",
                            hugepages_dir.display()
                        );
                        flowey::shell_cmd!(rt, "sh -c {write_overcommit_script}").run()?;
                        let configured = fs_err::read_to_string(
                            hugepages_dir.join("nr_overcommit_hugepages"),
                        )?
                        .trim()
                        .parse::<u64>()?;
                        anyhow::ensure!(
                            configured >= overcommit_pages,
                            "2 MiB hugetlb overcommit remains {configured}, below requested {overcommit_pages}"
                        );
                    }
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
                    args.push(profile.cli_name().into());
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
