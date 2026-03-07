// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    prost_build::Config::new()
        .compile_protos(&["src/tdisp.proto"], &["src/"])
        .unwrap();
}
