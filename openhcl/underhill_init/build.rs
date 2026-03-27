// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    build_rs_git_info::emit_git_info().unwrap();
}
