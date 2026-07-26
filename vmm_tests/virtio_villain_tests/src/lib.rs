// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Library half of the virtio-villain OpenVMM test runner: villain test
//! enumeration, serial verdict parsing, the OpenVMM known-failure list, and the
//! per-test VM driver.
//!
//! The nextest harness entrypoint lives in the `tests/villain.rs`
//! `harness = false` `[[test]]` target, which enumerates the villain
//! `tests.tsv` into one libtest-mimic trial per test.

pub mod known_failures;
pub mod run;
pub mod villain;
