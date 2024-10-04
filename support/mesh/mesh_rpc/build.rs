// Copyright (C) Microsoft Corporation. All rights reserved.

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator))
        .compile_protos(&["examples/example.proto"], &["examples/"])
        .unwrap();

    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .compile_protos(
            &["src/google/rpc/code.proto", "src/google/rpc/status.proto"],
            &["src/"],
        )
        .unwrap();
}
