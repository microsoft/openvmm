// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementations of various Xtasks

mod clean;
mod fmt;
mod fuzz;
mod git_hooks;
mod guest_test;
mod verify_size;

pub use git_hooks::update_hooks;

pub use self::clean::Clean;
pub use self::fmt::Fmt;
pub use self::fuzz::Fuzz;
pub use self::git_hooks::InstallGitHooks;
pub use self::git_hooks::RunGitHook;
pub use self::guest_test::GuestTest;
pub use self::verify_size::VerifySize;

/// CLI completion functions for variious Xtasks
pub mod cli_completions {
    /// CLI completion functions for `xtask fuzz`
    pub mod fuzz {
        pub(crate) use super::super::fuzz::complete_fuzzer_targets;
    }
}
