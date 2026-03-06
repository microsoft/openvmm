// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines top-level "job nodes" which can be composed when defining a flowey
//! pipeline using [`flowey::pipeline::prelude::PipelineJob::dep_on`].

pub mod all_good_job;
pub mod build_and_publish_openhcl_igvm_from_recipe;
pub mod build_and_publish_openvmm_hcl_baseline;
pub mod build_and_run_doc_tests;
pub mod build_and_run_nextest_unit_tests;
pub mod build_openhcl_igvm_from_recipe_nix;
pub mod build_test_results_website;
pub mod cfg_common;
pub mod cfg_gh_azure_login;
pub mod cfg_hvlite_reposource;
pub mod cfg_nix;
pub mod cfg_versions;
pub mod check_clippy;
pub mod check_openvmm_hcl_size;
pub mod check_xtask_fmt;
pub mod consolidate_and_publish_gh_pages;
pub mod consume_and_test_nextest_unit_tests_archive;
pub mod consume_and_test_nextest_vmm_tests_archive;
pub mod local_build_and_run_nextest_vmm_tests;
pub mod local_build_igvm;
pub mod local_custom_vmfirmwareigvm_dll;
pub mod local_restore_packages;
pub mod publish_vmgstool_gh_release;
pub mod test_local_flowey_build_igvm;
