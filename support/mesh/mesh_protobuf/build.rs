// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
fn main() {
    #[cfg(feature = "prost")]
    {
        prost_build::Config::new()
            .type_attribute(".", "#[derive(mesh_derive::Protobuf)]")
            .type_attribute(".", "#[mesh(prost)]")
            .compile_protos(&["src/prost.proto"], &["src/"])
            .unwrap();
    }
}
