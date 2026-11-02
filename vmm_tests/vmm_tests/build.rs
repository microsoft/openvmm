// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configure the guest architecture for VMM and backend contract tests.

fn main() {
    build_rs_guest_arch::emit_guest_arch();
}
