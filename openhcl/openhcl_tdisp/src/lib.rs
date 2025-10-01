// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//!
//! WARNING: *** This crate is a work in progress, do not use in production! ***
//!
//! This module provides an implementation of the TDISP client device
//! interface for OpenHCL devices.
//!
//! See: `vm/tdisp` for more information.

#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(missing_docs)]

use openhcl_tdisp_resources::VpciTdispInterface;
use tdisp::GuestToHostCommand;
use tdisp::GuestToHostResponse;
use tdisp::TdispCommandId;
use tdisp::TdispCommandResponsePayload;
