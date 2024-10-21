// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    vergen::EmitBuilder::builder().all_git().emit().unwrap();
}
