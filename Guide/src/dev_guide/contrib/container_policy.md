# ContainerPolicy: Adding a new container product

OpenHCL has an optional measured VTL2 payload called **ContainerPolicy**.
When the IGVM file is built with a container policy configured, the
payload is appended in-place after [`ParavisorMeasuredVtl2Config`] on
the same measured config region. The struct carries a
`container_policy_size: u32` field that tells the runtime exactly how
many bytes follow; a value of zero means absent. The build picks the
region's page count to fit `sizeof(struct) + policy_size`, up to a
hard cap of `PARAVISOR_MEASURED_VTL2_CONFIG_MAX_PAGES`.

At runtime, OpenHCL reads the struct, then reads the next
`container_policy_size` bytes and mesh-decodes them into the strongly
typed [`ContainerPolicy`] enum, or refuses to boot if the bytes are
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
   (such as CWCOW's `custom_uefi_json` base64 decoder) are inherently
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

When a manifest field needs build-side processing (base64 decoding,
shape conversion, etc.), attach a `#[serde(deserialize_with = "…")]`
adapter to the *field*. The wire type stays a single struct; only the
field's JSON shape diverges from its byte shape. CWCOW does this for
`custom_uefi_json`:

```rust
#[mesh(7)]
#[cfg_attr(
    feature = "manifest",
    serde(default, deserialize_with = "decode_custom_uefi_json_base64")
)]
pub custom_uefi_json: Vec<u8>,
```

The adapter is gated behind the `manifest` feature so the runtime crate
stays minimal:

```rust
#[cfg(feature = "manifest")]
fn decode_custom_uefi_json_base64<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use base64::Engine as _;
    let s = String::deserialize(d)?;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
}
```

In manifest JSON the field is a base64-encoded string (standard RFC
4648 alphabet, padding optional); in the wire bytes it is the decoded
raw payload. Embedding the bytes inline keeps manifests self-contained
and avoids out-of-band file dependencies during the build.

## Region layout

`ParavisorMeasuredVtl2Config` carries a `container_policy_size: u32`
field at offset `[16..20]`. The build records the encoded policy length
in this field; the runtime reads exactly that many bytes from
`CONTAINER_POLICY_INLINE_OFFSET` and mesh-decodes them. There is no
length-prefix framing — the struct field IS the framing.

```
0..8         ParavisorMeasuredVtl2Config.magic
8            vtom_offset_bit
9..16        padding
16..20       container_policy_size: u32  (0 ⇒ absent)
20..24       reserved
24..24+N     mesh_protobuf-encoded ContainerPolicy (N = container_policy_size)
24+N..end    zero padding to the next page boundary
```

The struct is 24 bytes; the region's *actual* page count is computed at
build time by `measured_vtl2_config_pages_for_policy(N)`. An absent
policy occupies exactly `PARAVISOR_MEASURED_VTL2_CONFIG_MIN_PAGES` (= 1)
page, identical to legacy builds. A larger policy grows the region up
to `PARAVISOR_MEASURED_VTL2_CONFIG_MAX_PAGES` pages.

The GPA-space reservation in the parameter region is sized for the
maximum (4 pages today); the IGVM file only imports the pages actually
needed. Builds that don't enable the policy import a single zero-padded
page — byte-for-byte identical to pre-feature builds.

## Measurement implications

Enabling ContainerPolicy alters the IGVM measurement because new
measured bytes are added. Existing recipes do **not** opt in by default,
so IGVMs built from those recipes import the same single zero-padded
page as before and preserve their prior measurements.

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
