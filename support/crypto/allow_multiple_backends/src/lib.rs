// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate exists solely to enable crypto's `allow-multiple-backends` feature for workspace-wide builds.
//! Without it commands like `cargo check --workspace` would fail due to binaries enabling multiple backends.
