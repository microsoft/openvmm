// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    vergen::EmitBuilder::builder().all_git().emit().unwrap();
}
