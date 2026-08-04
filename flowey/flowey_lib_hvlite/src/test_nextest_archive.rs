// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a cargo-nextest archive with a caller-provided test environment.

use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        pub friendly_name: String,
        pub nextest_archive_file: ReadVar<PathBuf>,
        pub nextest_filter_expr: Option<String>,
        pub nextest_profile: NextestProfile,
        pub nextest_working_dir: Option<ReadVar<PathBuf>>,
        pub nextest_config_file: Option<ReadVar<PathBuf>>,
        pub nextest_bin: Option<ReadVar<PathBuf>>,
        pub target: Option<ReadVar<target_lexicon::Triple>>,
        pub run_ignored: bool,
        pub extra_env: ReadVar<BTreeMap<String, String>>,
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        pub hugetlb_2mb_overcommit_pages: Option<u64>,
        pub prepare_vhost_vsock: bool,
        pub results: WriteVar<TestResults>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_nextest_run::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            friendly_name,
            nextest_archive_file,
            nextest_filter_expr,
            nextest_profile,
            nextest_working_dir,
            nextest_config_file,
            nextest_bin,
            target,
            run_ignored,
            mut extra_env,
            mut pre_run_deps,
            hugetlb_2mb_overcommit_pages,
            prepare_vhost_vsock,
            results,
        } = request;

        if hugetlb_2mb_overcommit_pages.is_some() {
            extra_env = extra_env.map(ctx, |mut env| {
                env.insert("OPENVMM_REQUIRE_2MB_HUGETLB".into(), "1".into());
                env
            });
        }

        if !matches!(ctx.backend(), FlowBackend::Local)
            && matches!(ctx.platform(), FlowPlatform::Linux(_))
        {
            pre_run_deps.push(
                ctx.emit_rust_step("ensure hypervisor device is accessible", |_| {
                    |rt| {
                        if Path::new("/dev/kvm").exists() {
                            flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/kvm").run()?;
                        }
                        if Path::new("/dev/mshv").exists() {
                            flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/mshv").run()?;
                        }
                        Ok(())
                    }
                }),
            );

            if let Some(overcommit_pages) = hugetlb_2mb_overcommit_pages {
                pre_run_deps.push(ctx.emit_rust_step(
                    "ensure 2 MiB hugetlb pages are available",
                    move |_| {
                        move |rt| {
                            let hugepages_dir =
                                Path::new("/sys/kernel/mm/hugepages/hugepages-2048kB");
                            let read_counter = |name: &str| -> anyhow::Result<u64> {
                                let value = fs_err::read_to_string(hugepages_dir.join(name))?;
                                Ok(value.trim().parse()?)
                            };
                            let write_overcommit_script = format!(
                                "echo {overcommit_pages} | sudo tee {path}",
                                path = hugepages_dir.join("nr_overcommit_hugepages").display(),
                            );
                            flowey::shell_cmd!(rt, "sh -c {write_overcommit_script}").run()?;
                            let nr_overcommit_hugepages = read_counter("nr_overcommit_hugepages")?;
                            if nr_overcommit_hugepages < overcommit_pages {
                                anyhow::bail!(
                                    "2 MiB hugetlb overcommit remains {}, below requested {}",
                                    nr_overcommit_hugepages,
                                    overcommit_pages
                                );
                            }
                            Ok(())
                        }
                    },
                ));
            }

            if prepare_vhost_vsock {
                pre_run_deps.push(ctx.emit_rust_step("prepare vhost-vsock", |_| {
                    |rt| {
                        flowey::shell_cmd!(rt, "sudo modprobe vhost_vsock").run()?;
                        for _ in 0..50 {
                            if Path::new("/dev/vhost-vsock").exists() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        if !Path::new("/dev/vhost-vsock").exists() {
                            anyhow::bail!(
                                "/dev/vhost-vsock did not appear after loading vhost_vsock"
                            );
                        }
                        flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/vhost-vsock").run()?;
                        Ok(())
                    }
                }));
            }
        }

        ctx.req(crate::run_cargo_nextest_run::Request {
            friendly_name,
            run_kind: flowey_lib_common::run_cargo_nextest_run::NextestRunKind::RunFromArchive {
                archive_file: nextest_archive_file,
                target,
                nextest_bin,
            },
            nextest_profile,
            nextest_filter_expr,
            nextest_working_dir,
            nextest_config_file,
            run_ignored,
            extra_env: Some(extra_env),
            pre_run_deps,
            results,
        });

        Ok(())
    }
}
