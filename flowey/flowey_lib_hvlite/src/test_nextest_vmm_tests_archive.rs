// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest based VMM tests from a pre-built archive.
//!
//! NOTE: The caller is responsible for setting `extra_env` and
//! `pre_run_deps` to ensure that all tests filtered by
//! `nextest_filter_expr` are able to run successfully.

use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Pre-built VMM tests nextest archive
        pub nextest_archive_file: ReadVar<NextestVmmTestsArchive>,
        /// nextest filter expression for what VMM tests to run
        pub nextest_filter_expr: Option<String>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,
        /// Nextest working directory (defaults to repo root)
        pub nextest_working_dir: Option<ReadVar<PathBuf>>,
        /// Nextest configuration file (defaults to config in repo)
        pub nextest_config_file: Option<ReadVar<PathBuf>>,
        /// Optionally provide the nextest bin to use
        pub nextest_bin: Option<ReadVar<PathBuf>>,
        /// Target for the tests to run on
        pub target: Option<ReadVar<target_lexicon::Triple>>,
        /// Additional env vars set when executing the tests.
        pub extra_env: ReadVar<BTreeMap<String, String>>,
        /// Wait for specified side-effects to resolve before building / running
        /// any tests. (e.g: to allow for some ambient packages / dependencies
        /// to get installed).
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        /// If set, configure this 2 MiB hugetlb surplus page overcommit limit before running tests.
        pub hugetlb_2mb_overcommit_pages: Option<u64>,
        /// Results of running the tests
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
            nextest_archive_file,
            nextest_filter_expr,
            nextest_profile,
            nextest_working_dir,
            nextest_config_file,
            nextest_bin,
            target,
            mut extra_env,
            mut pre_run_deps,
            hugetlb_2mb_overcommit_pages,
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
            pre_run_deps.push({
                ctx.emit_rust_step("ensure hypervisor device is accessible", |_| {
                    |rt| {
                        // Make whichever hypervisor device exists accessible.
                        // KVM machines have /dev/kvm, MSHV machines have /dev/mshv.
                        if Path::new("/dev/kvm").exists() {
                            flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/kvm").run()?;
                        }
                        if Path::new("/dev/mshv").exists() {
                            flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/mshv").run()?;
                        }
                        Ok(())
                    }
                })
            });

            if let Some(overcommit_pages) = hugetlb_2mb_overcommit_pages {
                pre_run_deps.push({
                    ctx.emit_rust_step("ensure 2 MiB hugetlb pages are available", move |_| {
                        move |rt| {
                            let hugepages_dir =
                                Path::new("/sys/kernel/mm/hugepages/hugepages-2048kB");

                            let read_counter = |name: &str| -> anyhow::Result<u64> {
                                let path = hugepages_dir.join(name);
                                let value = fs_err::read_to_string(&path)?;
                                Ok(value.trim().parse()?)
                            };

                            let write_overcommit_script = format!(
                                "echo {overcommit_pages} | sudo tee {path}",
                                path = hugepages_dir.join("nr_overcommit_hugepages").display(),
                            );
                            flowey::shell_cmd!(rt, "sh -c {write_overcommit_script}").run()?;

                            let nr_hugepages = read_counter("nr_hugepages")?;
                            let free_hugepages = read_counter("free_hugepages")?;
                            let nr_overcommit_hugepages = read_counter("nr_overcommit_hugepages")?;
                            let surplus_hugepages = read_counter("surplus_hugepages")?;

                            log::info!("2 MiB hugetlb nr_hugepages={nr_hugepages}");
                            log::info!("2 MiB hugetlb free_hugepages={free_hugepages}");
                            log::info!(
                                "2 MiB hugetlb nr_overcommit_hugepages={nr_overcommit_hugepages}"
                            );
                            log::info!("2 MiB hugetlb surplus_hugepages={surplus_hugepages}");

                            if nr_overcommit_hugepages < overcommit_pages {
                                anyhow::bail!(
                                    "2 MiB hugetlb overcommit remains {}, below requested {}",
                                    nr_overcommit_hugepages,
                                    overcommit_pages
                                );
                            }

                            Ok(())
                        }
                    })
                });
            }
        }

        let nextest_archive = nextest_archive_file.map(ctx, |x| x.archive_file);

        ctx.req(crate::run_cargo_nextest_run::Request {
            friendly_name: "vmm_tests".into(),
            run_kind: flowey_lib_common::run_cargo_nextest_run::NextestRunKind::RunFromArchive {
                archive_file: nextest_archive,
                target,
                nextest_bin,
            },
            nextest_profile,
            nextest_filter_expr,
            nextest_working_dir,
            nextest_config_file,
            run_ignored: false,
            extra_env: Some(extra_env),
            pre_run_deps,
            results,
        });

        Ok(())
    }
}
