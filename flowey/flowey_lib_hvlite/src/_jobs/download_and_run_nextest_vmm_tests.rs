// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built artifacts and run cargo-nextest based VMM tests.
//
// Unfortunately, this follows a different pattern than the normal VMM tests
// flow where VmmTestsBuiltArtifacts is constructed at the pipeline level.
// This is necessary because flowey does not yet support dependencies within
// a job.
// TODO: unify these two paths for running VMM tests in CI.

use crate::_jobs::consume_and_test_nextest_vmm_tests_archive::TestContentConfig;
use crate::common::CommonTriple;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifacts;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsWrite;
use crate::init_vmm_tests_content_dir::VmmTestsPreBuiltArtifactsSelections;
use crate::init_vmm_tests_content_dir::vmm_tests_artifact_builders::VmmTestsArtifactsBuilderWindowsX86;
use crate::init_vmm_tests_env::PetriParams;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDepsWindows;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use petri_artifacts_vmm_test::ErasedVmmTestImage;
use petri_artifacts_vmm_test::artifacts::test_iso;
use petri_artifacts_vmm_test::artifacts::test_vhd;
use petri_artifacts_vmm_test::artifacts::test_vmgs;

#[derive(Serialize, Deserialize)]
pub enum VmmTestsProfile {
    X64WindowsAll,
}

struct VmmTestsParameters {
    target: CommonTriple,
    nextest_filter_expr: String,
    built_artifacts: VmmTestsBuiltArtifacts,
    built_artifacts_write: VmmTestsBuiltArtifactsWrite,
    downloaded_artifacts: Vec<ErasedVmmTestImage>,
    prep_steps_variants: Vec<String>,
    external_deps: VmmTestsExternalDeps,
}

impl VmmTestsProfile {
    fn parameters(&self, ctx: &mut NodeCtx<'_>) -> VmmTestsParameters {
        match self {
            VmmTestsProfile::X64WindowsAll => {
                let (built_artifacts, built_artifacts_write) =
                    VmmTestsArtifactsBuilderWindowsX86::selections().new_var_set(ctx);
                let mut nextest_filter_expr = "all()".to_string();
                // self-hosted runners don't have HvlDeviceHost installed
                // TODO: configure runners and remove this exclusion
                nextest_filter_expr.push_str(" & !test(storvsp_nvme_hyperv)");
                // self-hosted runners don't have a Hyper-V switch configured
                // TODO: configure runners and remove this exclusion
                nextest_filter_expr.push_str(" & !test(dio_nic)");

                VmmTestsParameters {
                    target: CommonTriple::X86_64_WINDOWS_MSVC,
                    nextest_filter_expr,
                    built_artifacts,
                    built_artifacts_write,
                    downloaded_artifacts: vec![
                        test_vhd::ALPINE_3_23_X64.into(),
                        test_vhd::FREE_BSD_13_2_X64.into(),
                        test_iso::FREE_BSD_13_2_X64.into(),
                        test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64.into(),
                        test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64.into(),
                        test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64.into(),
                        test_vhd::UBUNTU_2404_SERVER_X64.into(),
                        test_vhd::UBUNTU_2504_SERVER_X64.into(),
                        test_vmgs::VMGS_WITH_BOOT_ENTRY.into(),
                        test_vmgs::VMGS_WITH_16K_TPM.into(),
                    ],
                    prep_steps_variants: vec!["standard".into(), "no-vmbus".into()],
                    external_deps: VmmTestsExternalDeps::Windows(VmmTestsExternalDepsWindows {
                        hyperv: true,
                        whp: true,
                        hardware_isolation: true,
                    }),
                }
            }
        }
    }
}

flowey_request! {
    pub struct Params {
        pub label: String,
        pub profile: VmmTestsProfile,
        pub repetitions: std::num::NonZeroU64,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_vmm_tests_built_artifacts::Node>();
        ctx.import::<crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            label,
            profile,
            repetitions,
            done,
        } = request;

        let VmmTestsParameters {
            target,
            nextest_filter_expr,
            built_artifacts,
            built_artifacts_write,
            downloaded_artifacts,
            prep_steps_variants,
            external_deps,
        } = profile.parameters(ctx);

        ctx.req(crate::download_vmm_tests_built_artifacts::Request {
            built_artifacts_write,
        });

        ctx.req(
            crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Params {
                junit_test_label: format!("{label}-vmm-tests"),
                target: target.as_triple(),
                nextest_profile: NextestProfile::Ci,
                nextest_filter_expr: Some(nextest_filter_expr),
                test_content_config: TestContentConfig::Uninitialized {
                    test_content_dir: None,
                    built_artifacts,
                    prebuilt_artifacts: VmmTestsPreBuiltArtifactsSelections {
                        // TODO: figure out which are actually needed
                        test_linux_initrd_x64: true,
                        test_linux_kernel_x64: true,
                        test_linux_initrd_aarch64: true,
                        test_linux_kernel_aarch64: true,
                        test_linux_bzimage_x64: true,
                        uefi_x64: true,
                        uefi_aarch64: false,
                        qemu_system_aarch64_linux_x64: false,
                    },
                    needs_virtio_win_drivers: true,
                    needs_release_igvm: true,
                },
                downloaded_artifacts,
                prep_steps_variants,
                external_deps,
                incubator_profile: None,
                upload_logs_on_success: false,
                fail_job_on_test_fail: true,
                repetitions,
                // TODO
                petri_params: PetriParams {
                    disable_remote_artifacts: true,
                    reuse_prepped_vhds: false,
                    require_2mb_hugetlb: false,
                },
                test_content_dir_as_repo_root: false,
                done,
            },
        );

        Ok(())
    }
}
