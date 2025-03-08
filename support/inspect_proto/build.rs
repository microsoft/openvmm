// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
use std::path::Path;

fn main() {
    let out_dir = std::env::var_os("OUT_DIR").unwrap();

    // Generate .proto files from the inspect types.
    mesh_protobuf::protofile::DescriptorWriter::new(&[
        mesh_protobuf::protofile::message_description::<inspect::Node>(),
    ])
    .write_to_path(&out_dir)
    .unwrap();

    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(
            mesh_build::MeshServiceGenerator::new()
                // For easy of integration with inspect, use mesh types instead
                // of prost types for inspect responses.
                .replace_type("InspectResponse", "InspectResponse2")
                .replace_type("UpdateResponse", "UpdateResponse2"),
        ))
        .compile_protos(
            &["src/inspect_service.proto"],
            &[Path::new("src"), out_dir.as_ref()],
        )
        .unwrap();
}
