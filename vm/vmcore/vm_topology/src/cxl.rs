// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL topology types.

/// CFMWS window restrictions bitfield.
///
/// Re-exported from `cxl_spec` so existing `vm_topology::cxl` users remain
/// source-compatible.
pub use cxl_spec::CfmwsWindowRestrictions;
