// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for the TDISP guest-to-host interface.

/// Unit tests for serialization and deserialization of TDISP guest-to-host commands and responses.
pub mod serialize_tests;

/// Unit tests for the TDISP state machine handling.
pub mod statemachine_tests;

/// End-to-end tests that drive the emulator through serialized command packets.
pub mod endtoend_tests;
