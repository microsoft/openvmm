// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Kmsg-related definitions shared by underhill_core and underhill_init.

#![forbid(unsafe_code)]

/// system is unusable
pub const LOGLEVEL_EMERG: u8 = 0;
/// action must be taken immediately
pub const LOGLEVEL_ALERT: u8 = 1;
/// critical conditions
pub const LOGLEVEL_CRIT: u8 = 2;
/// error conditions
pub const LOGLEVEL_ERR: u8 = 3;
/// warning conditions
pub const LOGLEVEL_WARNING: u8 = 4;
/// normal but significant condition
pub const LOGLEVEL_NOTICE: u8 = 5;
/// informational
pub const LOGLEVEL_INFO: u8 = 6;
/// debug-level messages
pub const LOGLEVEL_DEBUG: u8 = 7;

/// underhill_init user-mode log facility
pub const UNDERHILL_INIT_KMSG_FACILITY: u8 = 2;
/// underhill user-mode log facility
pub const UNDERHILL_KMSG_FACILITY: u8 = 3;
