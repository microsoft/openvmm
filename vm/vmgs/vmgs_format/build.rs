fn main() {
    #[cfg(feature = "proto")]
    prost_build::Config::new()
        .compile_protos(&["src/disk_table.proto"], &["src/"])
        .unwrap();
}
