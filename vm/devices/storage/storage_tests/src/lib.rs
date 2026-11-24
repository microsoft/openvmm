// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared test helpers and integration tests for the OpenVMM storage stack.
//!
//! The integration tests themselves live in `tests/`. This library holds the
//! pieces that more than one of them — or a backend crate's own unit tests —
//! needs to share.

#![forbid(unsafe_code)]

pub mod sector_range;
