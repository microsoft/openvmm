// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run commands in disposable Linux environments using Petri VM backends.
//!
//! Profiles select a VM launcher independently of the guest boot environment.
//! Petri owns VM launch, agent connections, logs, and teardown;
//! Incubator owns command execution, terminal control, and path/environment
//! mapping. Registered VMM tests use Petri's native nested-test support.

#![forbid(unsafe_code)]

mod bootstrap;
mod path_mapping;
mod profile;
mod qemu;
mod run;

/// Guest path where the host share is mounted.
pub const GUEST_SHARE_ROOT: &str = "/share";

pub use path_mapping::HostPathMapper;
pub use path_mapping::guest_env_from_incubator_env;
pub use profile::Arch;
pub use profile::IncubatorBackend;
pub use profile::IncubatorBoot;
pub use profile::IncubatorProfile;
pub use run::IncubatorConfig;
pub use run::IncubatorOutput;
pub use run::run_in_incubator;
