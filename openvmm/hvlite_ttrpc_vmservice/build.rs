// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator::new()))
        .compile_protos(&["src/vmservice.proto"], &["src"])
        .unwrap();

    println!("cargo:rerun-if-changed=src/vmservice.proto");
}
