// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A "standard library" of flowey nodes, not tied to any particular project.
//!
//! These nodes can be considered "building blocks" that project-specific flows
//! can leverage to quickly get up and running.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod _util;
pub mod ado_task_azure_key_vault;
pub mod ado_task_npm_authenticate;
pub mod ado_task_nuget_authenticate;
pub mod ado_task_nuget_tool_installer;
pub mod ado_task_publish_test_results;
pub mod cache;
pub mod cfg_cargo_common_flags;
pub mod cfg_persistent_dir_cargo_install;
pub mod check_needs_relaunch;
pub mod copy_to_artifact_dir;
pub mod download_azcopy;
pub mod download_cargo_fuzz;
pub mod download_cargo_nextest;
pub mod download_gh_artifact;
pub mod download_gh_cli;
pub mod download_gh_release;
pub mod download_mdbook;
pub mod download_mdbook_admonish;
pub mod download_mdbook_mermaid;
pub mod download_nuget_exe;
pub mod gen_cargo_nextest_run_cmd;
pub mod gh_download_azure_key_vault_secret;
pub mod gh_latest_completed_workflow_id;
pub mod gh_task_azure_login;
pub mod gh_workflow_id;
pub mod git_checkout;
pub mod git_merge_commit;
pub mod install_azure_cli;
pub mod install_cargo_nextest;
pub mod install_dist_pkg;
pub mod install_git;
pub mod install_nix;
pub mod install_nodejs;
pub mod install_nuget_azure_credential_provider;
pub mod install_rust;
pub mod nix_deps_provider;
pub mod nuget_install_package;
pub mod publish_test_results;
pub mod resolve_protoc;
pub mod run_cargo_build;
pub mod run_cargo_clippy;
pub mod run_cargo_doc;
pub mod run_cargo_nextest_archive;
pub mod run_cargo_nextest_run;
pub mod use_gh_cli;
