// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! flowey nodes specific to the HvLite project.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod _jobs;
pub mod artifact_openhcl_igvm_from_recipe;
pub mod artifact_openhcl_igvm_from_recipe_extras;
pub mod artifact_openvmm_hcl_sizecheck;
pub mod build_and_test_vmgs_lib;
pub mod build_guest_test_uefi;
pub mod build_guide;
pub mod build_hypestv;
pub mod build_igvmfilegen;
pub mod build_nextest_unit_tests;
pub mod build_nextest_vmm_tests;
pub mod build_ohcldiag_dev;
pub mod build_openhcl_boot;
pub mod build_openhcl_igvm_from_recipe;
pub mod build_openhcl_initrd;
pub mod build_openvmm;
pub mod build_openvmm_hcl;
pub mod build_pipette;
pub mod build_prep_steps;
pub mod build_rustdoc;
pub mod build_sidecar;
pub mod build_test_igvm_agent_rpc_server;
pub mod build_tmk_vmm;
pub mod build_tmks;
pub mod build_tpm_guest_tests;
pub mod build_vmfirmwareigvm_dll;
pub mod build_vmgstool;
pub mod build_xtask;
pub mod cfg_openvmm_magicpath;
pub mod cfg_rustup_version;
pub mod download_openvmm_vmm_tests_artifacts;
pub mod download_release_igvm_files_from_gh;
pub mod download_uefi_mu_msvm;
pub mod git_checkout_openvmm_repo;
pub mod init_cross_build;
pub mod init_openvmm_cargo_config_deny_warnings;
pub mod init_openvmm_magicpath_linux_test_kernel;
pub mod init_openvmm_magicpath_openhcl_sysroot;
pub mod init_openvmm_magicpath_protoc;
pub mod init_openvmm_magicpath_release_openhcl_igvm;
pub mod init_openvmm_magicpath_uefi_mu_msvm;
pub mod init_vmm_tests_env;
pub mod install_git_credential_manager;
pub mod install_openvmm_rust_build_essential;
pub mod install_vmm_tests_deps;
pub mod resolve_openhcl_kernel_package;
pub mod resolve_openvmm_deps;
pub mod run_cargo_build;
pub mod run_cargo_nextest_run;
pub mod run_igvmfilegen;
pub mod run_prep_steps;
pub mod run_split_debug_info;
pub mod run_test_igvm_agent_rpc_server;
pub mod stop_test_igvm_agent_rpc_server;
pub mod test_nextest_unit_tests_archive;
pub mod test_nextest_vmm_tests_archive;
