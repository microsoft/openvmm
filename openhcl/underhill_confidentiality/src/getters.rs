// Copyright (C) Microsoft Corporation. All rights reserved.

use std::sync::OnceLock;

static CONFIDENTIAL: OnceLock<bool> = OnceLock::new();
static CONFIDENTIAL_DEBUG: OnceLock<bool> = OnceLock::new();

fn get_bool_env_var(name: &str) -> bool {
    std::env::var_os(name).map_or(false, |v| !v.is_empty() && v != "0")
}

/// Gets whether the current VM is a confidential VM.
///
/// Generally, accessing this information through the HCL ioctl is preferred.
pub fn is_confidential_vm() -> bool {
    *CONFIDENTIAL.get_or_init(|| get_bool_env_var(crate::UNDERHILL_CONFIDENTIAL_ENV_VAR_NAME))
}

/// Gets whether confidential debugging is enabled. This is an IGVM-level setting,
/// intended to allow for disabling diagnostic restrictions on CVMs.
pub fn confidential_debug_enabled() -> bool {
    *CONFIDENTIAL_DEBUG
        .get_or_init(|| get_bool_env_var(crate::UNDERHILL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME))
}

/// Gets whether confidential filtering is enabled. This is the source of truth for
/// whether diagnostic sources should filter their output to enforce confidentiality.
pub fn confidential_filtering_enabled() -> bool {
    is_confidential_vm() && !confidential_debug_enabled()
}
