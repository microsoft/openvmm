// Copyright (C) Microsoft Corporation. All rights reserved.

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator))
        .compile_protos(&["src/vmservice.proto"], &["src"])
        .unwrap();

    println!("cargo:rerun-if-changed=src/vmservice.proto");
}
