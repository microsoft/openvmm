# ContainerPolicy: Adding a new container product

OpenHCL has an optional measured VTL2 page called **ContainerPolicy**. When
the IGVM file is built with a container policy configured, the page is
imported as a measured (and therefore attestable) page carrying a
product-specific policy body. At runtime, OpenHCL reads the page (via a
pointer in [`ParavisorMeasuredVtl2Config`]) and decodes it back into the
strongly-typed [`ContainerPolicy`] enum, or refuses to boot if the page is
malformed.

The first real product is **CWCOW** (Confidential Windows Container
Optimized Workload). The bundled `X64CvmCwcow` recipe + the two
`openhcl-x64-cvm-cwcow-{dev,release}.json` manifests are the canonical
end-to-end example.

## Architecture in one diagram

```
manifest JSON  ──serde::Deserialize──▶  ContainerPolicy (wire enum)
                                              │
                                              │ mesh_protobuf::encode
                                              ▼
                                  framed bytes on measured page
                                              │
                                              │ runtime decode
                                              ▼
                                  ContainerPolicy (same type)
```

The wire enum and the runtime enum are literally the same Rust type
(`loader_defs::paravisor::ContainerPolicy`). The mesh oneof tag identifies
the product on the wire; the compiler enforces that each variant carries
its strongly-typed body. There is **no separate `product_id` field**, no
parser trait, and no central dispatch.

## Hard rules

1. **Never reuse a `#[mesh(N)]` tag.** Once allocated to a product, the
   number is permanent — re-using it would silently change the measured
   wire format for an existing product.
2. **Never derive `serde::Serialize`** on `ContainerPolicy` or any
   `*Policy` body. Field-level `#[serde(deserialize_with)]` adapters
   (such as CWCOW's `custom_uefi_json` path reader) are inherently
   asymmetric — a symmetric Serialize impl would silently round-trip
   wire bytes back to JSON instead of the original input shape.

## Adding a new product

The default flow is two edits in `vm/loader/loader_defs/src/paravisor.rs`:

```rust
/// 1. Define a body struct (mesh + optional serde::Deserialize under
///    the `manifest` feature). Manifest field names match wire field
///    names by default; use serde(rename) / deserialize_with on
///    individual fields when needed.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Deserialize))]
#[cfg_attr(
    feature = "manifest",
    serde(rename_all = "snake_case", deny_unknown_fields)
)]
#[mesh(package = "openhcl.container_policy")]
pub struct FooPolicy {
    #[mesh(1)] pub setting_a: bool,
    #[mesh(2)] pub setting_b: u32,
    // Add new #[mesh(N)] fields later; mesh treats them as optional.
}

/// 2. Add a variant to the wire enum with a fresh #[mesh(N)] tag.
pub enum ContainerPolicy {
    #[mesh(1)] Cwcow(CwcowPolicy),
    #[mesh(2)] Foo(FooPolicy),
}
```

Manifest authors can then write:

```json
"container_policy": { "foo": { "setting_a": true, "setting_b": 7 } }
```

…and the manifest deserializes directly into the wire enum.

### Build-time work on individual fields

When a manifest field needs build-side processing (file I/O, base64
decoding, etc.), attach a `#[serde(deserialize_with = "…")]` adapter to
the *field*. The wire type stays a single struct; only the field's JSON
shape diverges from its byte shape. CWCOW does this for
`custom_uefi_json`:

```rust
#[mesh(7)]
#[cfg_attr(
    feature = "manifest",
    serde(default, deserialize_with = "read_custom_uefi_json_path")
)]
pub custom_uefi_json: Vec<u8>,
```

The adapter is gated behind the `manifest` feature so the runtime crate
stays minimal:

```rust
#[cfg(feature = "manifest")]
fn read_custom_uefi_json_path<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = std::path::PathBuf::deserialize(d)?;
    std::fs::read(&path).map_err(|e| serde::de::Error::custom(format!(...)))
}
```

In manifest JSON the field is a path string; in the wire bytes it is
the file contents.

## Wire framing

The measured page payload starts with a 4-byte little-endian `u32` length
prefix followed by the `mesh_protobuf`-encoded `ContainerPolicy`. The
length prefix is required because mesh_protobuf does not natively tolerate
the trailing zero padding that the IGVM importer pads to a page boundary.
Use `encode_container_policy_page` / `decode_container_policy_page` from
`loader_defs::paravisor` so both ends share the same framing helpers.

## Measurement implications

Enabling ContainerPolicy alters the IGVM measurement because new measured
page contents are added. Existing recipes do **not** opt in by default,
so IGVMs built from those recipes preserve their prior measurements. The
region's address-space reservation is the same regardless of whether a
policy is configured, so a build's downstream layout is stable.

The page location is recorded in
`ParavisorMeasuredVtl2Config::container_policy_location`, a packed
`(page_index, page_count)` `u64` (low 52 bits + high 12 bits). A
`page_count` of zero means absent — older IGVMs that pre-date the field
read as zero because the page is zero-padded to 4 KiB before measurement.

## Optional: recipe + manifest

If you want the new product reachable from `cargo xflowey build-igvm`:

- Add an `OpenhclIgvmRecipe::*` variant in
  `flowey/flowey_lib_hvlite/src/build_openhcl_igvm_from_recipe.rs`
  pointing at dev/release manifests.
- Add the matching `OpenhclRecipeCli::*` variant in
  `flowey/flowey_hvlite/src/pipelines/build_igvm.rs`.
- Add a manifest JSON under `vm/loader/manifests/` that sets
  `image.openhcl.container_policy`.
- Wire the filename mappings in
  `flowey/flowey_lib_hvlite/src/artifact_openhcl_igvm_from_recipe.rs` and
  `flowey/flowey_lib_hvlite/src/_jobs/local_build_igvm.rs`.

The bundled `X64CvmCwcow` recipe shows the complete CWCOW pipeline.

[`ParavisorMeasuredVtl2Config`]: https://openvmm.dev/rustdoc/loader_defs/paravisor/struct.ParavisorMeasuredVtl2Config.html
[`ContainerPolicy`]: https://openvmm.dev/rustdoc/loader_defs/paravisor/enum.ContainerPolicy.html
