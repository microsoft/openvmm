// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
use std::path::Path;
use std::path::PathBuf;

pub fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let out_dir: PathBuf = std::env::var_os("OUT_DIR").unwrap().into();

    minify_json_in_folder(Path::new("./templates/aarch64"), &out_dir.join("aarch64"));
    minify_json_in_folder(Path::new("./templates/x64"), &out_dir.join("x64"));
}

fn minify_json_in_folder(in_folder: &Path, out_folder: &Path) {
    std::fs::create_dir_all(out_folder).unwrap();

    for e in std::fs::read_dir(in_folder).unwrap() {
        let e = e.unwrap();
        if !e.file_type().unwrap().is_file() {
            panic!("{} must only contain .json files", in_folder.display());
        }

        if let Some(ext) = e.path().extension() {
            if ext != "json" {
                panic!("{} must only contain .json files", in_folder.display());
            }
        }

        let input = std::fs::File::open(e.path()).unwrap();
        let output = std::fs::File::create(out_folder.join(e.file_name())).unwrap();

        let data: serde_json::Value = serde_json::from_reader(input).unwrap();
        serde_json::to_writer(output, &data).unwrap();
    }
}
