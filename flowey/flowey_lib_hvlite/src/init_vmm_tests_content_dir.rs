// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setup directory structure that the VMM tests require to run.

use crate::build_flowey_hvlite::FloweyHvliteOutput;
use crate::build_guest_test_uefi::GuestTestUefiOutput;
use crate::build_incubator::IncubatorOutput;
use crate::build_incubator::incubator_profile_dir;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openvmm::OpenvmmOutput;
use crate::build_openvmm_vhost::OpenvmmVhostOutput;
use crate::build_pipette::PipetteOutput;
use crate::build_prep_steps::PrepStepsOutput;
use crate::build_test_igvm_agent_rpc_server::TestIgvmAgentRpcServerOutput;
use crate::build_tmk_vmm::TmkVmmOutput;
use crate::build_tmks::TmksOutput;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::build_vmgstool::VmgstoolOutput;
use crate::common::CommonArch;
use crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion;
use flowey::node::prelude::*;
use petri_artifacts_common::artifacts::*;
use petri_artifacts_core::ArtifactId;
use petri_artifacts_core::ArtifactTarget;
use petri_artifacts_core::AsArtifactHandle;
use petri_artifacts_vmm_test::artifacts::*;

macro_rules! define_vmm_tests_built_artifacts {
    (
        $name:ident,
        $($artifact:ident(
            $($variant:ident(
                ($output_let_expr:expr, $member:ident $(, $none_member:ident)*),
                $artifact_ty:ty
            )),* $(,)?
        ) => $output:ty),* $(,)?
    ) => {
        ::paste::paste! {
            #[derive(Serialize, Deserialize)]
            pub struct $name<C = VarNotClaimed> {$($(
                pub [<$artifact _ $variant>]: Option<::flowey::node::prelude::ReadVar<$output, C>>,
            )*)*}

            impl Default for $name<VarNotClaimed> {
                fn default() -> Self {
                    Self {$($(
                        [<$artifact _ $variant>]: None,
                    )*)*}
                }
            }

            impl $name<VarNotClaimed> {
                pub fn claim(self, ctx: &mut StepCtx<'_>) -> $name<VarClaimed> {
                    let Self {$($(
                        [<$artifact _ $variant>],
                    )*)*} = self;
                    $name {$($(
                        [<$artifact _ $variant>]: [<$artifact _ $variant>].claim(ctx),
                    )*)*}
                }

                $(pub fn $artifact(&mut self, target: ArtifactTarget) -> ::anyhow::Result<&mut Option<::flowey::node::prelude::ReadVar<$output>>> {
                    match &target {
                        $($artifact_ty::TARGET => Ok(&mut self.[<$artifact _ $variant>]),)*
                        _ if let Some(target) = retry_as_musl(target.clone()) => {
                            return self.$artifact(target);
                        }
                        _ => Err(anyhow::anyhow!("{} {}", target, concat!("does not exist for ", stringify!($artifact)))),
                    }
                })*

                $($(pub fn [<$artifact _ $variant _target>]() -> ::target_lexicon::Triple {
                    $artifact_ty::TARGET.target_triple().expect("no target triple for artifact")
                })*)*
            }

            impl $name<VarClaimed> {
                pub fn write(self, rt: &mut RustRuntimeServices<'_>, test_content_dir: impl AsRef<Path>) -> ::anyhow::Result<()> {
                    let Self {$($(
                        [<$artifact _ $variant>],
                    )*)*} = self;


                    $($(if let Some(artifact) = [<$artifact _ $variant>] {
                        let dst = test_content_dir
                            .as_ref()
                            .join($artifact_ty::relative_path());

                        #[expect(clippy::allow_attributes)]
                        #[allow(irrefutable_let_patterns, unused_variables)]
                        let $output_let_expr = rt.read(artifact) else {
                            ::anyhow::bail!(concat!(
                                "unexpected variant of ",
                                stringify!($output),
                                " for ",
                                stringify!([<$artifact _ $variant>])
                            ));
                        };

                        fs_err::create_dir_all(dst.parent().unwrap())?;
                        fs_err::copy($member, &dst)?;
                        dst.make_executable()?;
                    })*)*

                    Ok(())
                }
            }

            #[derive(Serialize, Deserialize)]
            pub struct [<$name Write>]<C = VarNotClaimed> {$($(
                pub [<$artifact _ $variant>]: Option<::flowey::node::prelude::WriteVar<$output, C>>,
            )*)*}

            impl Default for [<$name Write>]<VarNotClaimed> {
                fn default() -> Self {
                    Self {$($(
                        [<$artifact _ $variant>]: None,
                    )*)*}
                }
            }

            impl [<$name Write>]<VarNotClaimed> {
                pub fn claim(self, ctx: &mut StepCtx<'_>) -> [<$name Write>]<VarClaimed> {
                    let Self {$($(
                        [<$artifact _ $variant>],
                    )*)*} = self;
                    [<$name Write>] {$($(
                        [<$artifact _ $variant>]: [<$artifact _ $variant>].claim(ctx),
                    )*)*}
                }
            }

            impl [<$name Write>]<VarClaimed> {
                pub fn resolve(self, rt: &mut RustRuntimeServices<'_>, test_content_dir: impl AsRef<Path>) -> ::anyhow::Result<()> {
                    let Self {$($(
                        [<$artifact _ $variant>],
                    )*)*} = self;


                    $($(if let Some(artifact) = [<$artifact _ $variant>] {
                        let $member = test_content_dir
                            .as_ref()
                            .join($artifact_ty::relative_path());

                        if $member.exists() {
                            $(let $none_member = None;)*
                            rt.write(
                                artifact,
                                &$output_let_expr,
                            )
                        } else {
                            anyhow::bail!("{} not found", $member.display());
                        }
                    })*)*

                    Ok(())
                }
            }

            #[derive(Serialize, Deserialize, Default, Debug)]
            pub struct [<$name Selections>] {$($(
                pub [<$artifact _ $variant>]: bool,
            )*)*}

            impl [<$name Selections>] {
                pub fn resolve_artifact(&mut self, id: &str) -> bool {
                    match id {
                        $($($artifact_ty::GLOBAL_UNIQUE_ID => {
                            self.[<$artifact _ $variant>] = true;
                            true
                        })*)*
                        _ => false
                    }
                }

                $(pub fn [<$artifact _for>](&self, target: ArtifactTarget) -> ::anyhow::Result<bool>{
                    match &target {
                        $($artifact_ty::TARGET => {
                            Ok(self.[<$artifact _ $variant>])
                        })*
                        _ => Err(anyhow::anyhow!("{} {}", target, concat!("does not exist for ", stringify!($artifact)))),
                    }
                }

                pub fn [<require_ $artifact _for>](&mut self, target: ArtifactTarget) -> ::anyhow::Result<PathBuf>{
                    match &target {
                        $($artifact_ty::TARGET => {
                            self.[<$artifact _ $variant>] = true;
                            Ok($artifact_ty::relative_path())
                        })*
                        _ if let Some(target) = retry_as_musl(target.clone()) => {
                            self.[<require_ $artifact _for>](target)
                        }
                        _ => Err(anyhow::anyhow!("{} {}", target, concat!("does not exist for ", stringify!($artifact)))),
                    }
                })*

                pub fn new_var_set(&self, ctx: &mut ::flowey::node::prelude::NodeCtx<'_>) -> (
                    $name,
                    [<$name Write>],
                ) {
                    let mut built_artifacts_read = $name::default();
                    let mut built_artifacts_write = [<$name Write>]::default();

                    $($(if self.[<$artifact _ $variant>] {
                        let (read_var, write_var) = ctx.new_var();
                        built_artifacts_read.[<$artifact _ $variant>] = Some(read_var);
                        built_artifacts_write.[<$artifact _ $variant>] = Some(write_var);
                    })*)*

                    (built_artifacts_read, built_artifacts_write)
                }

                pub fn handles(&self) -> Vec<::petri_artifacts_core::ErasedArtifactHandle> {
                    let mut handles = Vec::new();

                    $($(if self.[<$artifact _ $variant>] {
                        handles.push($artifact_ty.erase())
                    })*)*

                    handles
                }
            }

        }
    };
}

// Try again with musl target, since some artifacts may only be built for musl
// to save time. Binaries targeting musl can also run in gnu environments.
fn retry_as_musl(target: ArtifactTarget) -> Option<ArtifactTarget> {
    let ArtifactTarget::Triple(mut triple) = target else {
        return None;
    };

    if matches!(
        triple.operating_system,
        target_lexicon::OperatingSystem::Linux
    ) && !matches!(triple.environment, target_lexicon::Environment::Musl)
    {
        triple.environment = target_lexicon::Environment::Musl;
        Some(ArtifactTarget::Triple(triple))
    } else {
        None
    }
}

define_vmm_tests_built_artifacts!(
    VmmTestsBuiltArtifacts,
    // Artifacts used at the pipeline level.
    flowey_hvlite(
        windows_x64(
            (FloweyHvliteOutput::WindowsBin { exe, pdb }, exe, pdb),
            host_tools::FLOWEY_HVLITE_WINDOWS_X64
        ),
        windows_aarch64(
            (FloweyHvliteOutput::WindowsBin { exe, pdb }, exe, pdb),
            host_tools::FLOWEY_HVLITE_WINDOWS_AARCH64
        ),
        linux_x64(
            (FloweyHvliteOutput::LinuxBin { bin, dbg}, bin, dbg),
            host_tools::FLOWEY_HVLITE_LINUX_X64
        ),
    ) => FloweyHvliteOutput,
    nextest_vmm_tests_archive(
        windows_x64(
            (NextestVmmTestsArchive { archive_file }, archive_file),
            host_tools::NEXTEST_VMM_TESTS_ARCHIVE_WINDOWS_X64
        ),
        windows_aarch64(
            (NextestVmmTestsArchive { archive_file }, archive_file),
            host_tools::NEXTEST_VMM_TESTS_ARCHIVE_WINDOWS_AARCH64
        ),
        linux_x64(
            (NextestVmmTestsArchive { archive_file }, archive_file),
            host_tools::NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64
        ),
        linux_musl_x64(
            (NextestVmmTestsArchive { archive_file }, archive_file),
            host_tools::NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64_MUSL
        ),
        linux_musl_aarch64(
            (NextestVmmTestsArchive { archive_file }, archive_file),
            host_tools::NEXTEST_VMM_TESTS_ARCHIVE_LINUX_AARCH64_MUSL
        ),
    ) => NextestVmmTestsArchive,
    incubator(
        linux_x64(
            (IncubatorOutput { bin, dbg}, bin, dbg),
            host_tools::INCUBATOR_LINUX_X64
        ),
    ) => IncubatorOutput,
    prep_steps(
        windows_x64(
            (PrepStepsOutput::WindowsBin { exe, pdb }, exe, pdb),
            host_tools::PREP_STEPS_WINDOWS_X64
        ),
        linux_musl_x64(
            (PrepStepsOutput::LinuxBin { bin, dbg}, bin, dbg),
            host_tools::PREP_STEPS_LINUX_X64_MUSL
        ),
    ) => PrepStepsOutput,
    test_igvm_agent_rpc_server(
        windows_x64(
            (TestIgvmAgentRpcServerOutput { exe, pdb }, exe, pdb),
            host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64
        ),
    ) => TestIgvmAgentRpcServerOutput,

    // Artifacts used internally by petri.
    openvmm(
        windows_x64(
            (OpenvmmOutput::WindowsBin { exe, pdb }, exe, pdb),
            OPENVMM_WINDOWS_X64
        ),
        windows_aarch64(
            (OpenvmmOutput::WindowsBin { exe, pdb }, exe, pdb),
            OPENVMM_WINDOWS_AARCH64
        ),
        linux_x64(
            (OpenvmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            OPENVMM_LINUX_X64
        ),
        linux_aarch64(
            (OpenvmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            OPENVMM_LINUX_AARCH64
        ),
        linux_musl_x64(
            (OpenvmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            OPENVMM_LINUX_X64_MUSL
        ),
        linux_musl_aarch64(
            (OpenvmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            OPENVMM_LINUX_AARCH64_MUSL
        ),
    ) => OpenvmmOutput,
    openvmm_vhost(
        linux_x64(
            (OpenvmmVhostOutput { bin, dbg}, bin, dbg),
            OPENVMM_VHOST_LINUX_X64
        ),
        linux_aarch64(
            (OpenvmmVhostOutput { bin, dbg}, bin, dbg),
            OPENVMM_VHOST_LINUX_AARCH64
        ),
        linux_musl_x64(
            (OpenvmmVhostOutput { bin, dbg}, bin, dbg),
            OPENVMM_VHOST_LINUX_X64_MUSL
        ),
        linux_musl_aarch64(
            (OpenvmmVhostOutput { bin, dbg}, bin, dbg),
            OPENVMM_VHOST_LINUX_AARCH64_MUSL
        ),
    ) => OpenvmmVhostOutput,
    pipette(
        windows_x64(
            (PipetteOutput::WindowsBin { exe, pdb }, exe, pdb),
            PIPETTE_WINDOWS_X64
        ),
        windows_aarch64(
            (PipetteOutput::WindowsBin { exe, pdb }, exe, pdb),
            PIPETTE_WINDOWS_AARCH64
        ),
        linux_musl_x64(
            (PipetteOutput::LinuxBin { bin, dbg}, bin, dbg),
            PIPETTE_LINUX_X64_MUSL
        ),
        linux_musl_aarch64(
            (PipetteOutput::LinuxBin { bin, dbg}, bin, dbg),
            PIPETTE_LINUX_AARCH64_MUSL
        ),
    ) => PipetteOutput,
    guest_test_uefi(
        x64(
            (GuestTestUefiOutput { img, efi, pdb }, img, efi, pdb),
            test_vhd::GUEST_TEST_UEFI_X64
        ),
        aarch64(
            (GuestTestUefiOutput { img, efi, pdb }, img, efi, pdb),
            test_vhd::GUEST_TEST_UEFI_AARCH64
        ),
    ) => GuestTestUefiOutput,
    openhcl_standard(
        x64(
            (OpenhclIgvmOutput::X64 { igvm_bin }, igvm_bin),
            openhcl_igvm::LATEST_STANDARD_X64
        ),
        aarch64(
            (OpenhclIgvmOutput::Aarch64 { igvm_bin }, igvm_bin),
            openhcl_igvm::LATEST_STANDARD_AARCH64
        ),
    ) => OpenhclIgvmOutput,
    openhcl_standard_dev(
        x64(
            (OpenhclIgvmOutput::X64Devkern { igvm_bin }, igvm_bin),
            openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_X64
        ),
        aarch64(
            (OpenhclIgvmOutput::Aarch64Devkern { igvm_bin }, igvm_bin),
            openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_AARCH64
        ),
    ) => OpenhclIgvmOutput,
    openhcl_cvm(
        x64(
            (OpenhclIgvmOutput::X64Cvm { igvm_bin, endorsements }, igvm_bin, endorsements),
            openhcl_igvm::LATEST_CVM_X64
        ),
    ) => OpenhclIgvmOutput,
    openhcl_linux_direct(
        x64(
            (OpenhclIgvmOutput::X64TestLinuxDirect { igvm_bin }, igvm_bin),
            openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64
        ),
    ) => OpenhclIgvmOutput,
    tmks(
        x64(
            (TmksOutput { bin, dbg}, bin, dbg),
            tmks::SIMPLE_TMK_X64
        ),
        aarch64(
            (TmksOutput { bin, dbg}, bin, dbg),
            tmks::SIMPLE_TMK_AARCH64
        ),
    ) => TmksOutput,
    tmk_vmm(
        windows_x64(
            (TmkVmmOutput::WindowsBin { exe, pdb }, exe, pdb),
            tmks::TMK_VMM_WINDOWS_X64
        ),
        windows_aarch64(
            (TmkVmmOutput::WindowsBin { exe, pdb }, exe, pdb),
            tmks::TMK_VMM_WINDOWS_AARCH64
        ),
        linux_musl_x64(
            (TmkVmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            tmks::TMK_VMM_LINUX_X64_MUSL
        ),
        linux_musl_aarch64(
            (TmkVmmOutput::LinuxBin { bin, dbg}, bin, dbg),
            tmks::TMK_VMM_LINUX_AARCH64_MUSL
        ),
    ) => TmkVmmOutput,
    vmgstool(
        windows_x64(
            (VmgstoolOutput::WindowsBin { exe, pdb }, exe, pdb),
            vmgstool::VMGSTOOL_WINDOWS_X64
        ),
        windows_aarch64(
            (VmgstoolOutput::WindowsBin { exe, pdb }, exe, pdb),
            vmgstool::VMGSTOOL_WINDOWS_AARCH64
        ),
        linux_x64(
            (VmgstoolOutput::LinuxBin { bin, dbg}, bin, dbg),
            vmgstool::VMGSTOOL_LINUX_X64
        ),
    ) => VmgstoolOutput,
    vmgstool_dev(
        windows_x64(
            (VmgstoolOutput::WindowsBin { exe, pdb }, exe, pdb),
            vmgstool::VMGSTOOL_DEV_WINDOWS_X64
        ),
        windows_aarch64(
            (VmgstoolOutput::WindowsBin { exe, pdb }, exe, pdb),
            vmgstool::VMGSTOOL_DEV_WINDOWS_AARCH64
        ),
        linux_x64(
            (VmgstoolOutput::LinuxBin { bin, dbg}, bin, dbg),
            vmgstool::VMGSTOOL_DEV_LINUX_X64
        ),
    ) => VmgstoolOutput,
    tpm_guest_tests(
        windows_x64(
            (TpmGuestTestsOutput::WindowsBin { exe, pdb }, exe, pdb),
            guest_tools::TPM_GUEST_TESTS_WINDOWS_X64
        ),
        linux_x64(
            (TpmGuestTestsOutput::LinuxBin { bin, dbg}, bin, dbg),
            guest_tools::TPM_GUEST_TESTS_LINUX_X64
        ),
    ) => TpmGuestTestsOutput,
);

pub type ResolveVmmTestsBuiltArtifacts =
    Box<dyn Fn(&mut flowey::pipeline::prelude::PipelineJobCtx<'_>) -> VmmTestsBuiltArtifacts>;

#[macro_export]
macro_rules! vmm_tests_built_artifacts_builder {
    (
        $name:ty,
        (
            $($artifact:ident => $output:ty),* $(,)?
        )
    ) => {
        ::paste::paste! {
            #[derive(Default, Clone)]
            pub struct $name {
                $(pub [<use_ $artifact>]: Option<::flowey::pipeline::prelude::UseTypedArtifact<$output>>,)*
            }

            impl $name {
                pub fn finish(self) -> Result<$crate::init_vmm_tests_content_dir::ResolveVmmTestsBuiltArtifacts, &'static str> {
                    let $name {
                        $([<use_ $artifact>],)*
                    } = self;

                    $(let [<use_ $artifact>] = [<use_ $artifact>].ok_or(stringify!($artifact))?;)*

                    Ok(Box::new(move |ctx| $crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifacts {
                        $($artifact: Some(ctx.use_typed_artifact(&[<use_ $artifact>])),)*
                        .. Default::default()
                    }))
                }

                pub fn selections() -> $crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsSelections {
                    let mut selections = $crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsSelections::default();
                    $(selections.$artifact = true;)*
                    selections
                }
            }
        }
    };
}

define_vmm_tests_built_artifacts!(
    VmmTestsPreBuiltArtifacts,
    test_linux_kernel(
        x64(
            (src, src),
            loadable::LINUX_DIRECT_TEST_KERNEL_X64
        ),
        aarch64(
            (src, src),
            loadable::LINUX_DIRECT_TEST_KERNEL_AARCH64
        ),
    ) => PathBuf,
    test_linux_initrd(
        x64(
            (src, src),
            loadable::LINUX_DIRECT_TEST_INITRD_X64
        ),
        aarch64(
            (src, src),
            loadable::LINUX_DIRECT_TEST_INITRD_AARCH64
        ),
    ) => PathBuf,
    test_linux_bzimage(
        x64(
            (src, src),
            loadable::LINUX_DIRECT_TEST_BZIMAGE_X64
        ),
    ) => PathBuf,
    uefi(
        x64(
            (src, src),
            loadable::UEFI_FIRMWARE_X64
        ),
        aarch64(
            (src, src),
            loadable::UEFI_FIRMWARE_AARCH64
        ),
    ) => PathBuf,
    qemu_system_aarch64(
        linux_x64(
            (src, src),
            QEMU_SYSTEM_AARCH64_LINUX_X64
        ),
    ) => PathBuf,
);

flowey_request! {
    pub struct Request {
        /// Directory to symlink / copy test contents into. Does not need to be
        /// empty.
        pub test_content_dir: ReadVar<PathBuf>,
        /// What triple VMM tests are built for.
        ///
        /// Used to detect cases of running Windows VMM tests via WSL2, and adjusting
        /// reported paths appropriately.
        pub vmm_tests_target: target_lexicon::Triple,
        /// Artifacts used by the tests that are built in the openvmm repo
        pub built_artifacts: VmmTestsBuiltArtifacts,
        /// Artifacts to download that are pre-built as part of OpenVMM deps
        pub prebuilt_artifacts: VmmTestsPreBuiltArtifactsSelections,
        /// Copy files necessary to use the test content dir as a minimal repo root.
        ///
        /// This is useful for running tests on machines without a local clone.
        pub is_repo_root: bool,
        /// Whether to copy incubator profiles into the test content directory.
        pub needs_incubator_profiles: bool,

        // TODO: refactor these last two to use one artifact per arch so that
        // they can be part of `VmmTestsPreBuiltArtifactsSelections`.
        pub needs_virtio_win_drivers: bool,
        pub needs_release_igvm: bool,

        pub done: WriteVar<SideEffect>
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::resolve_openvmm_test_virtio_win::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::download_release_igvm_files_from_gh::resolve::Node>();
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            vmm_tests_target,
            built_artifacts,
            prebuilt_artifacts,
            is_repo_root,
            needs_incubator_profiles,
            needs_virtio_win_drivers,
            needs_release_igvm,
            done,
        } = request;

        let openvmm_repo_path =
            is_repo_root.then(|| ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir));

        let arch = CommonArch::from_architecture(vmm_tests_target.architecture)?;

        let test_linux_initrd_x64 = prebuilt_artifacts.test_linux_initrd_x64.then(|| {
            ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::X86_64, v))
        });
        let test_linux_initrd_aarch64 = prebuilt_artifacts.test_linux_initrd_aarch64.then(|| {
            ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::Aarch64, v))
        });
        let test_linux_kernel_x64 = prebuilt_artifacts.test_linux_kernel_x64.then(|| {
            ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                    CommonArch::X86_64,
                    crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION,
                    v,
                )
            })
        });
        let test_linux_kernel_aarch64 = prebuilt_artifacts.test_linux_kernel_aarch64.then(|| {
            ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                    CommonArch::Aarch64,
                    crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION,
                    v,
                )
            })
        });
        let test_linux_bzimage_x64 = prebuilt_artifacts.test_linux_bzimage_x64.then(|| {
            ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::BzImage,
                    CommonArch::X86_64,
                    crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION,
                    v,
                )
            })
        });

        let uefi_x64 = prebuilt_artifacts.uefi_x64.then(|| {
            ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
                arch: CommonArch::X86_64,
                msvm_fd: v,
            })
        });
        let uefi_aarch64 = prebuilt_artifacts.uefi_aarch64.then(|| {
            ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
                arch: CommonArch::Aarch64,
                msvm_fd: v,
            })
        });

        let qemu_system_aarch64_linux_x64 =
            prebuilt_artifacts.qemu_system_aarch64_linux_x64.then(|| {
                ctx.reqv(|v| {
                    crate::resolve_openvmm_qemu::Request::Get(
                        crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                        CommonArch::X86_64,
                        v,
                    )
                })
            });

        let prebuilt_artifacts = VmmTestsPreBuiltArtifacts {
            test_linux_kernel_x64,
            test_linux_kernel_aarch64,
            test_linux_initrd_x64,
            test_linux_initrd_aarch64,
            test_linux_bzimage_x64,
            uefi_x64,
            uefi_aarch64,
            qemu_system_aarch64_linux_x64,
        };

        let virtio_win_dir = needs_virtio_win_drivers
            .then(|| ctx.reqv(crate::resolve_openvmm_test_virtio_win::Request::Get));

        let release_igvm_files = needs_release_igvm.then(|| {
            ctx.reqv(
                |v| crate::download_release_igvm_files_from_gh::resolve::Request {
                    arch,
                    release_igvm_files: v,
                    release_version: OpenhclReleaseVersion::latest(),
                },
            )
        });

        ctx.emit_rust_step("setting up vmm_tests content dir", |ctx| {
            claim_vars!(
                ctx,
                (
                    test_content_dir,
                    openvmm_repo_path,
                    // built artifacts
                    built_artifacts,
                    // downloaded artifacts
                    prebuilt_artifacts,
                    virtio_win_dir,
                    release_igvm_files,
                )
            );

            done.claim(ctx);

            move |rt| {
                read_vars!(rt, (release_igvm_files, test_content_dir));

                if !test_content_dir.exists() {
                    fs_err::create_dir_all(&test_content_dir)?
                };

                if let Some(openvmm_repo_path) = openvmm_repo_path {
                    let openvmm_repo_path = rt.read(openvmm_repo_path);

                    let nextest_config_file = PathBuf::new().join(".config").join("nextest.toml");
                    fs_err::create_dir_all(
                        test_content_dir
                            .join(&nextest_config_file)
                            .parent()
                            .context("no parent")?,
                    )?;
                    fs_err::copy(
                        openvmm_repo_path.join(&nextest_config_file),
                        test_content_dir.join(&nextest_config_file),
                    )?;

                    let repo_cargo_toml_file = Path::new("Cargo.toml");
                    fs_err::copy(
                        openvmm_repo_path.join(repo_cargo_toml_file),
                        test_content_dir.join(repo_cargo_toml_file),
                    )?;

                    let crate_cargo_toml_file = PathBuf::new()
                        .join("vmm_tests")
                        .join("vmm_tests")
                        .join("Cargo.toml");
                    fs_err::create_dir_all(
                        test_content_dir
                            .join(&crate_cargo_toml_file)
                            .parent()
                            .context("no parent")?,
                    )?;
                    fs_err::copy(
                        openvmm_repo_path.join(&crate_cargo_toml_file),
                        test_content_dir.join(&crate_cargo_toml_file),
                    )?;

                    if needs_incubator_profiles {
                        let incubator_profile_dir = incubator_profile_dir();
                        fs_err::create_dir_all(test_content_dir.join(&incubator_profile_dir))?;
                        for entry in
                            fs_err::read_dir(openvmm_repo_path.join(&incubator_profile_dir))?
                        {
                            let profile = entry?.path();
                            if profile.is_file()
                                && profile.extension().is_some_and(|ext| ext == "toml")
                            {
                                let dst = test_content_dir
                                    .join(&incubator_profile_dir)
                                    .join(profile.file_name().context("no file name")?);
                                fs_err::copy(profile, dst)?;
                            }
                        }
                    }
                }

                built_artifacts.write(rt, &test_content_dir)?;

                prebuilt_artifacts.write(rt, &test_content_dir)?;

                if let Some(release_igvm_files) = release_igvm_files {
                    if let Some(src) = &release_igvm_files.openhcl {
                        fs_err::copy(
                            src,
                            test_content_dir
                                .join(openhcl_igvm::LATEST_RELEASE_STANDARD_X64::relative_path()),
                        )?;
                    }

                    if let Some(src) = &release_igvm_files.openhcl_aarch64 {
                        fs_err::copy(
                            src,
                            test_content_dir.join(
                                openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64::relative_path(),
                            ),
                        )?;
                    }

                    if let Some(src) = &release_igvm_files.openhcl_direct {
                        fs_err::copy(
                            src,
                            test_content_dir.join(
                                openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64::relative_path(),
                            ),
                        )?;
                    }
                }

                if let Some(virtio_win_dir) = virtio_win_dir {
                    let src = rt.read(virtio_win_dir);
                    let dst =
                        test_content_dir.join(virtio_win::VIRTIO_WINDOWS_DRIVERS::relative_path());
                    let _ = fs_err::remove_dir_all(&dst);
                    flowey_lib_common::_util::copy_dir_all(&src, &dst)?;
                }

                // log the current contents of the dir
                log::info!("final folder content: {}", test_content_dir.display());
                fn list_dir(dir: PathBuf, layer: usize) -> anyhow::Result<()> {
                    if layer > 2
                        || dir
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| ["temp", "test_results"].contains(&n))
                    {
                        log::info!("{}- ...", " ".repeat(layer * 2));
                        return Ok(());
                    }
                    for entry in dir.read_dir()? {
                        let entry = entry?;
                        log::info!("{}- {}", " ".repeat(layer * 2), entry.file_name().display());
                        let subdir = entry.path();
                        if subdir.is_dir() {
                            list_dir(subdir, layer + 1)?;
                        }
                    }
                    Ok(())
                }
                list_dir(test_content_dir, 0)?;

                Ok(())
            }
        });

        Ok(())
    }
}

/// Utility builders which make it easy to "skim off" artifacts required by VMM
/// test execution from other pipeline jobs.
//
// DEVNOTE: this is pub so internal tests can reuse the same builders
pub mod vmm_tests_artifact_builders {
    use super::*;

    vmm_tests_built_artifacts_builder!(
        VmmTestsArtifactsBuilderLinuxX86,
        (
            // windows build machine
            pipette_windows_x64 => PipetteOutput,
            // linux build machine
            nextest_vmm_tests_archive_linux_x64 => NextestVmmTestsArchive,
            openvmm_linux_x64 => OpenvmmOutput,
            openvmm_vhost_linux_x64 => OpenvmmVhostOutput,
            pipette_linux_musl_x64 => PipetteOutput,
            pipette_linux_musl_aarch64 => PipetteOutput,
            prep_steps_linux_musl_x64 => PrepStepsOutput,
            tmk_vmm_linux_musl_x64 => TmkVmmOutput,
            // any machine
            guest_test_uefi_x64 => GuestTestUefiOutput,
            tmks_x64 => TmksOutput,
        )
    );

    vmm_tests_built_artifacts_builder!(
        VmmTestsArtifactsBuilderLinuxMuslX86,
        (
            // windows build machine
            pipette_windows_x64 => PipetteOutput,
            // linux build machine
            nextest_vmm_tests_archive_linux_musl_x64 => NextestVmmTestsArchive,
            openvmm_linux_musl_x64 => OpenvmmOutput,
            openvmm_vhost_linux_musl_x64 => OpenvmmVhostOutput,
            pipette_linux_musl_x64 => PipetteOutput,
            pipette_linux_musl_aarch64 => PipetteOutput,
            prep_steps_linux_musl_x64 => PrepStepsOutput,
            tmk_vmm_linux_musl_x64 => TmkVmmOutput,
            // any machine
            guest_test_uefi_x64 => GuestTestUefiOutput,
            tmks_x64 => TmksOutput,
        )
    );

    vmm_tests_built_artifacts_builder!(
        VmmTestsArtifactsBuilderWindowsX86,
        (
            // windows build machine
            nextest_vmm_tests_archive_windows_x64 => NextestVmmTestsArchive,
            openvmm_windows_x64 => OpenvmmOutput,
            pipette_windows_x64 => PipetteOutput,
            tmk_vmm_windows_x64 => TmkVmmOutput,
            prep_steps_windows_x64 => PrepStepsOutput,
            vmgstool_windows_x64 => VmgstoolOutput,
            vmgstool_dev_windows_x64 => VmgstoolOutput,
            tpm_guest_tests_windows_x64 => TpmGuestTestsOutput,
            test_igvm_agent_rpc_server_windows_x64 => TestIgvmAgentRpcServerOutput,
            // linux build machine
            openhcl_standard_x64 => OpenhclIgvmOutput,
            openhcl_cvm_x64 => OpenhclIgvmOutput,
            openhcl_linux_direct_x64 => OpenhclIgvmOutput,
            pipette_linux_musl_x64 => PipetteOutput,
            tmk_vmm_linux_musl_x64 => TmkVmmOutput,
            tpm_guest_tests_linux_x64 => TpmGuestTestsOutput,
            // any machine
            guest_test_uefi_x64 => GuestTestUefiOutput,
            tmks_x64 => TmksOutput,
        )
    );

    vmm_tests_built_artifacts_builder!(
        VmmTestsArtifactsBuilderWindowsAarch64,
        (
            // windows build machine
            nextest_vmm_tests_archive_windows_aarch64 => NextestVmmTestsArchive,
            openvmm_windows_aarch64 => OpenvmmOutput,
            pipette_windows_aarch64 => PipetteOutput,
            tmk_vmm_windows_aarch64 => TmkVmmOutput,
            vmgstool_windows_aarch64 => VmgstoolOutput,
            vmgstool_dev_windows_aarch64 => VmgstoolOutput,
            // linux build machine
            openhcl_standard_aarch64 => OpenhclIgvmOutput,
            pipette_linux_musl_aarch64 => PipetteOutput,
            tmk_vmm_linux_musl_aarch64 => TmkVmmOutput,
            // any machine
            guest_test_uefi_aarch64 => GuestTestUefiOutput,
            tmks_aarch64 => TmksOutput,
        )
    );

    // Artifact builder for aarch64 Linux VMM tests running via QEMU TCG.
    //
    // The test binaries are aarch64-linux-musl (run inside QEMU), but the
    // incubator binary is x86_64-linux-gnu (runs on the CI host).
    vmm_tests_built_artifacts_builder!(
        VmmTestsArtifactsBuilderLinuxAarch64Tcg,
        (
            // x86_64 CI host binary
            incubator_linux_x64 => IncubatorOutput,
            // aarch64 guest binaries
            nextest_vmm_tests_archive_linux_musl_aarch64 => NextestVmmTestsArchive,
            openvmm_linux_musl_aarch64 => OpenvmmOutput,
            pipette_linux_musl_aarch64 => PipetteOutput,
            guest_test_uefi_aarch64 => GuestTestUefiOutput,
            tmks_aarch64 => TmksOutput,
            tmk_vmm_linux_musl_aarch64 => TmkVmmOutput,
        )
    );
}
