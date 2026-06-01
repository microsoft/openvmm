# ProductPolicy: Adding a new product

OpenHCL has an optional measured VTL2 payload called **ProductPolicy**.
When the IGVM file is built with a product policy configured, the
payload is appended in-place after [`ParavisorMeasuredVtl2Config`] on
the same measured config region. The struct carries a
`product_policy_size: u32` field that tells the runtime exactly how
many bytes follow; a value of zero means absent. The region is a
fixed `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` pages (currently 1).
If a new policy's mesh-encoded body would overflow that budget, the
IGVM build hard-panics so a developer is forced to consciously bump
`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` — and accept the
attestation-measurement change for every IGVM built from that point on.

At runtime, OpenHCL reads the struct, then reads the next
`product_policy_size` bytes and mesh-decodes them into the strongly
typed [`ProductPolicy`] enum, or refuses to boot if the bytes are
malformed.

The first real product is **CWCOW** (Confidential Windows Container
on Windows). The bundled `X64CvmCwcow` recipe + the two
`openhcl-x64-cvm-cwcow-{dev,release}.json` manifests are the canonical
end-to-end example.

## Architecture in one diagram

```text
manifest JSON  ──serde::Deserialize──▶  ProductPolicy (wire enum)
                                              │
                                              │ mesh_protobuf::encode
                                              ▼
                                  mesh_protobuf-encoded bytes in measured config region
                                              │
                                              │ runtime decode
                                              ▼
                                  ProductPolicy (same type)
```

The wire enum and the runtime enum are literally the same Rust type
(`openhcl_product_policy::ProductPolicy`). The mesh oneof tag identifies
the product on the wire; the compiler enforces that each variant carries
its strongly-typed body. There is **no separate `product_id` field**, no
parser trait, and no central dispatch.

The public helper APIs are `openhcl_product_policy::encode_product_policy`
and `openhcl_product_policy::decode_product_policy`. The loader adds a
private `encode_product_policy_bytes` wrapper that enforces build-time
product invariants (see [Required fields and build-time
invariants](#required-fields-and-build-time-invariants)) and the
measured-region size limit.

## Current wire schema

`ProductPolicy` currently has one product variant:

| Mesh tag | Manifest key / Rust variant | Body type |
| --- | --- | --- |
| `1` | `cwcow` / `ProductPolicy::Cwcow` | `CwcowPolicy` |

`CwcowPolicy` currently contains:

| Mesh tag | Manifest field / Rust field | Type |
| --- | --- | --- |
| `1` | `vmgs_read_only` | `bool` |
| `2` | `require_secure_boot` | `bool` |
| `3` | `require_secure_boot_vars` | `bool` |
| `4` | `require_bcd_integrity` | `bool` |
| `5` | `require_secure_avic` | `bool` |
| `6` | `custom_uefi_json` | `Vec<u8>`; standard-base64 string in manifest JSON |

Both the enum and body struct use
`#[serde(rename_all = "snake_case", deny_unknown_fields)]` under the
`manifest` feature.

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

The default flow is two edits in `openhcl/openhcl_product_policy/src/wire.rs`:

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
#[mesh(package = "openhcl.product_policy")]
pub struct FooPolicy {
    #[mesh(1)] pub setting_a: bool,
    #[mesh(2)] pub setting_b: u32,
    // Add new #[mesh(N)] fields later; mesh treats them as optional.
}

/// 2. Add a variant to the wire enum with a fresh #[mesh(N)] tag.
pub enum ProductPolicy {
    #[mesh(1)] Cwcow(CwcowPolicy),
    #[mesh(2)] Foo(FooPolicy),
}
```

Manifest authors can then write:

```json
"product_policy": { "foo": { "setting_a": true, "setting_b": 7 } }
```

…and the manifest deserializes directly into the wire enum. The
matching `Serialize` impl keeps the manifest representation symmetric
with deserialization; the `json_round_trip_is_byte_identical` test
enforces that a wire-enum value can be serialized to JSON and
deserialized back without changing bytes.

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
    serde(with = "custom_uefi_json_serde")
)]
pub custom_uefi_json: Vec<u8>,
```

```rust
#[cfg(feature = "manifest")]
mod custom_uefi_json_serde {
    use base64::Engine as _;
    use serde::Deserialize as _;

    pub fn serialize<S: serde::Serializer>(
        bytes: &[u8],
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

`ParavisorMeasuredVtl2Config` carries a `product_policy_size: u32`
field at offset `[16..20]`. The build records the encoded policy length
in this field; the runtime reads exactly that many bytes from
`PRODUCT_POLICY_INLINE_OFFSET` and mesh-decodes them. There is no
length-prefix framing — the struct field IS the framing.

At runtime, size `0` maps to `None` and the decoder is not called. For
nonzero sizes, OpenHCL first rejects any `product_policy_size` larger
than `PRODUCT_POLICY_MAX_SIZE_BYTES`, then reads exactly that many
bytes and calls `decode_product_policy`. Malformed bytes are a hard
boot error.

```text
0..8         ParavisorMeasuredVtl2Config.magic
8            vtom_offset_bit
9..16        padding
16..20       product_policy_size: u32  (0 ⇒ absent)
20..24       reserved
24..24+N     mesh_protobuf-encoded ProductPolicy (N = product_policy_size)
24+N..end    zero padding to the end of the fixed SIZE_PAGES region
```

The struct is 24 bytes; the region occupies exactly
`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES * HV_PAGE_SIZE` bytes
(currently one 4 KiB page) regardless of whether a policy is
present. The struct sits at offset 0; the optional `product_policy_size`
bytes of mesh-encoded policy sit immediately after; the remainder is
zero-padded to the end of that fixed measured region.

Builds that don't enable the policy still import the same fixed
`SIZE_PAGES` measured region; the struct's `product_policy_size`
field is `0` and every trailing byte is zero. The measurement is fully
determined by `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES` plus the
struct contents, so any bump to `SIZE_PAGES` retroactively changes the
measurement of *every* IGVM built from this branch — including ones
without a configured policy.

If a future product's encoded policy exceeds the fixed
measured-config-region budget
(`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES * HV_PAGE_SIZE -
PRODUCT_POLICY_INLINE_OFFSET`, i.e. 4072 bytes today),
`encode_product_policy_bytes` will `panic!` at IGVM-build time with
a message that names `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`. The
fix is to bump that constant (e.g. from 1 to 2) in
`openhcl/openhcl_product_policy/src/wire.rs`. Bumping it is a measurement
change — every IGVM, with or without a configured policy, will have a
new measurement after the bump — so it must be reviewed against the
attestation policy for each affected product.

## Required fields and build-time invariants

CWCOW's manifest contract is intentionally strict: **every field of
`CwcowPolicy` must appear in the manifest JSON**. None of the booleans
have a serde default, and `custom_uefi_json` is now also mandatory (no
`#[serde(default)]`). Omitting any field is a deserialization error,
not a silent default.

In addition, `encode_product_policy_bytes` panics at IGVM build time
if `custom_uefi_json` is empty: the CWCOW product relies on the custom
UEFI JSON to lock down secure-boot variables and BCD integrity, so an
empty payload would produce an attested-but-meaningless image. New
products should enforce their own equivalent invariants in
`validate_product_policy_for_build`.

## Optional: recipe + manifest

If you want the new product reachable from `cargo xflowey build-igvm`:

- Add an `OpenhclIgvmRecipe::*` variant in
  `flowey/flowey_lib_hvlite/src/build_openhcl_igvm_from_recipe.rs`
  pointing at dev/release manifests.
- Add the matching `OpenhclRecipeCli::*` variant in
  `flowey/flowey_hvlite/src/pipelines/build_igvm.rs`.
- Add a manifest JSON under `vm/loader/manifests/` that sets
  `image.openhcl.product_policy`.
- Wire the filename mappings in
  `flowey/flowey_lib_hvlite/src/artifact_openhcl_igvm_from_recipe.rs` and
  `flowey/flowey_lib_hvlite/src/_jobs/local_build_igvm.rs`.

The bundled `X64CvmCwcow` recipe shows the complete CWCOW pipeline.

[`ParavisorMeasuredVtl2Config`]: https://openvmm.dev/rustdoc/openhcl_product_policy/struct.ParavisorMeasuredVtl2Config.html
[`ProductPolicy`]: https://openvmm.dev/rustdoc/openhcl_product_policy/enum.ProductPolicy.html
