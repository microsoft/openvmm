// Copyright (C) Microsoft Corporation. All rights reserved.

fn main() {
    vergen::EmitBuilder::builder().all_git().emit().unwrap();
}
