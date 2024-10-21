// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator))
        .compile_protos(&["src/profile.proto"], &["src/"])
        .unwrap();
}
