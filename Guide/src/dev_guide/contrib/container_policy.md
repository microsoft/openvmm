# ContainerPolicy: Adding a new container product

OpenHCL has an optional measured VTL2 payload called **ContainerPolicy**.
When the IGVM file is built with a container policy configured, the
payload is appended in-place after [`ParavisorMeasuredVtl2Config`] on
the same measured config region. The struct carries a
`container_policy_size: u32` field that tells the runtime exactly how
many bytes follow; a value of zero means absent. The region is a
fixed `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` pages (currently 2).
If a new policy's mesh-encoded body would overflow that budget, the
IGVM build hard-panics so a developer is forced to consciously bump
`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` — and accept the
attestation-measurement change for every IGVM built from that point on.

At runtime, OpenHCL reads the struct, then reads the next
`container_policy_size` bytes and mesh-decodes them into the strongly
typed [`ContainerPolicy`] enum, or refuses to boot if the bytes are
malformed.

The first real product is **CWCOW** (Confidential Windows Container
on Windows). The bundled `X64CvmCwcow` recipe + the two
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
2. **Any non-trivial field encoding must be a *symmetric* serde
   adapter.** When a manifest field's JSON shape differs from its wire
   byte shape (e.g. CWCOW's base64-encoded `custom_uefi_json`), use
   `#[serde(with = "module_name")]` with a helper module that exposes
   matching `serialize` *and* `deserialize` functions. Never use
   one-directional `#[serde(deserialize_with = "…")]` alone — it
   leaves the Serialize side free to emit a shape that won't
   deserialize again, silently corrupting any future manifest dump.
   The `json_round_trip_is_byte_identical` test enforces this for
   every existing field.

## Adding a new product

The default flow is two edits in `vm/loader/loader_defs/src/paravisor.rs`:

```rust
/// 1. Define a body struct (mesh + symmetric serde under the
///    `manifest` feature). Manifest field names match wire field
///    names by default.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
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

…and the manifest deserializes directly into the wire enum. The
matching `Serialize` impl lets build tooling re-emit a manifest from
a wire-enum value (useful for `igvmfilegen --dump-manifest` and
similar workflows).

### Custom field encoding (must be symmetric)

When a manifest field's JSON shape differs from its wire byte shape —
e.g. CWCOW embeds the custom UEFI JSON as a base64-encoded string —
attach a *symmetric* `#[serde(with = "…")]` adapter to the field. The
helper module supplies matching `serialize` and `deserialize`
functions so JSON round-trips are byte-stable:

```rust
#[mesh(6)]
#[cfg_attr(
    feature = "manifest",
    serde(default, with = "custom_uefi_json_serde")
)]
pub custom_uefi_json: Vec<u8>,
```

```rust
#[cfg(feature = "manifest")]
mod custom_uefi_json_serde {
    use base64::Engine as _;
    use serde::Deserialize as _;

    pub fn serialize<S: serde::Serializer>(
        bytes: &Vec<u8>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}
```

The adapter is gated behind the `manifest` feature so the runtime
crate stays minimal. The mandatory `json_round_trip_is_byte_identical`
test exercises this contract: any asymmetry breaks the build.

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
24+N..end    zero padding to the end of the fixed SIZE_PAGES region
```

The struct is 24 bytes; the region occupies exactly
`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES * HV_PAGE_SIZE` bytes
(currently two 4 KiB pages) regardless of whether a policy is
present. The struct sits at offset 0; the optional `container_policy_size`
bytes of mesh-encoded policy sit immediately after; the remainder is
zero-padded to the page boundary.

Builds that don't enable the policy still import the same fixed
`SIZE_PAGES` measured region; the struct's `container_policy_size`
field is `0` and every trailing byte is zero. The measurement is fully
determined by `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` plus the
struct contents, so any bump to `SIZE_PAGES` retroactively changes the
measurement of *every* IGVM built from this branch — including ones
without a configured policy.

If a future container product's encoded policy exceeds the per-page
budget (`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES * HV_PAGE_SIZE -
CONTAINER_POLICY_INLINE_OFFSET`, i.e. 8168 bytes today),
`encode_container_policy_bytes` will `panic!` at IGVM-build time with
a message that names `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`. The
fix is to bump that constant (e.g. from 2 to 3) in
`vm/loader/loader_defs/src/paravisor.rs`. Bumping it is a measurement
change — every IGVM, with or without a configured policy, will have a
new measurement after the bump — so it must be reviewed against the
attestation policy for each affected product.

## Measurement implications

Enabling ContainerPolicy alters the IGVM measurement because new
measured bytes are added. Recipes that don't opt in at all still see a
measurement change relative to pre-feature builds, because the measured
config region is now fixed at `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`
(currently 2). Within this branch, recipes without a policy stay at a
stable measurement as long as nobody bumps `SIZE_PAGES`; bumping
`SIZE_PAGES` to fit a larger policy is itself a measurement change for
every recipe, with or without a policy configured.

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
