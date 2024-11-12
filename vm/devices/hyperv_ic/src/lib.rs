// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of Hyper-V ICs (Integration Components).
//!
//! These are simple devices used to make simple requests to the guest or
//! otherwise provide some degree of integration between the guest and host.
//!
//! Examples (not all are necessarily implemented yet):
//!
//! * shutdown IC for initiating a guest shutdown
//! * timesync IC for synchronizing time
//! * heartbeat IC for reporting guest health
//! * KVP IC for exchanging arbitrary key/value data between the host and guest

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;
pub mod shutdown;
