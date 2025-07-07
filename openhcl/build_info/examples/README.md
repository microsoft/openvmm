# Build Info Examples

This directory contains examples of how to use the arbitrary build data feature.

## Basic usage

Set environment variables with the `OPENVMM_BUILD_` prefix and build:

```bash
export OPENVMM_BUILD_CUSTOM_1="internal_build_456"
export OPENVMM_BUILD_TIMESTAMP="2024-01-01T10:00:00Z"
export OPENVMM_BUILD_FEATURES="feature1,feature2"
export OPENVMM_BUILD_RUST_VERSION="1.88.0"

cargo build -p your_crate
```

In your code:

```rust
use build_info;

let build_info = build_info::get();

// Check build profile
if build_info.is_debug_build() {
    println!("This is a debug build");
}

// Get specific arbitrary data
if let Some(custom_data) = build_info.get_arbitrary_data("custom_1") {
    println!("Custom build data: {}", custom_data);
}

// List all arbitrary data
for (key, value) in build_info.arbitrary_data() {
    println!("{}: {}", key, value);
}
```

## Supported environment variables

- `OPENVMM_BUILD_TARGET` - Build target information
- `OPENVMM_BUILD_FEATURES` - Enabled features
- `OPENVMM_BUILD_TIMESTAMP` - Build timestamp
- `OPENVMM_BUILD_RUST_VERSION` - Rust version used for build
- `OPENVMM_BUILD_CUSTOM_1` through `OPENVMM_BUILD_CUSTOM_5` - Custom build data

## Internal builds

For internal builds, you can set these environment variables in your build system to include specific metadata without needing to modify the OSS codebase.