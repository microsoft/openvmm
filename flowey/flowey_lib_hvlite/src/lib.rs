// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! flowey nodes specific to the HvLite project.

#![forbid(unsafe_code)]
// #![warn(missing_docs)] // TODO: lots to do here

pub mod _jobs;
pub mod artifact_guest_test_uefi;
pub mod artifact_guide;
pub mod artifact_hypestv;
pub mod artifact_igvmfilegen;
pub mod artifact_nextest_unit_tests_archive;
pub mod artifact_nextest_vmm_tests_archive;
pub mod artifact_ohcldiag_dev;
pub mod artifact_openhcl_boot;
pub mod artifact_openhcl_igvm_from_recipe;
pub mod artifact_openhcl_igvm_from_recipe_extras;
pub mod artifact_openvmm;
pub mod artifact_openvmm_hcl_sizecheck;
pub mod artifact_pipette;
pub mod artifact_rustdoc;
pub mod artifact_vmgs_lib;
pub mod artifact_vmgstool;
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
pub mod build_rustdoc;
pub mod build_sidecar;
pub mod build_vmfirmwareigvm_dll;
pub mod build_vmgstool;
pub mod build_xtask;
pub mod cfg_openvmm_magicpath;
pub mod download_lxutil;
pub mod download_openhcl_kernel_package;
pub mod download_openvmm_deps;
pub mod download_openvmm_vmm_tests_vhds;
pub mod download_uefi_mu_msvm;
pub mod git_checkout_openvmm_repo;
pub mod init_cross_build;
pub mod init_hyperv_tests;
pub mod init_openvmm_cargo_config_deny_warnings;
pub mod init_openvmm_magicpath_linux_test_kernel;
pub mod init_openvmm_magicpath_lxutil;
pub mod init_openvmm_magicpath_openhcl_sysroot;
pub mod init_openvmm_magicpath_protoc;
pub mod init_openvmm_magicpath_uefi_mu_msvm;
pub mod init_vmm_tests_env;
pub mod install_git_credential_manager;
pub mod install_openvmm_rust_build_essential;
pub mod run_cargo_build;
pub mod run_cargo_nextest_run;
pub mod run_igvmfilegen;
pub mod run_split_debug_info;
pub mod test_nextest_unit_tests_archive;
pub mod test_nextest_vmm_tests_archive;
