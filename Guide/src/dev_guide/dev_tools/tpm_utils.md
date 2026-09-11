# tpm_utils

`tpm_utils` is a developer tool for preparing and inspecting pre-provisioned
vTPM NVRAM state blobs.

A vTPM's persistent state lives in a single opaque blob that the TPM reference
implementation manufactures and commits. OpenVMM and OpenHCL store that blob in
the VMGS file, under file ID `TPM_NVRAM` for TPM v1.38 or `TPM_185_NVRAM` for
TPM v1.85. `tpm_utils` drives a reference implementation directly, runs the
provisioning commands against it, and writes the resulting blob to disk so it
can be placed into a VMGS with [VmgsTool](./vmgstool.md).

This is useful for exercising the state import path at VM boot without needing
a real provisioning service.

```admonish warning
The TPM reference implementations need a crypto backend, so the tool must be
built with the `tpm` feature and is only supported where that backend is
available. Building the v1.85 backend also requires `cmake`.
```

## Crypto backend

The state a TPM commits is tied to the library that produced it, so `tpm_utils`
should be built against the same backend as the build the state is destined
for. Exactly one backend feature must be enabled alongside `tpm`:

| Feature | Backend |
| --- | --- |
| `openssl` | OpenSSL. Enabled by default. |
| `symcrypt` | [SymCrypt] for the v1.85 library. |

The v1.38 library has no SymCrypt backend, so it stays on OpenSSL even when
`symcrypt` is selected.

These features are non-additive — enabling both is a build error — so select
SymCrypt by turning off the default:

```bash
cargo run -p tpm_utils --no-default-features --features tpm,symcrypt -- ...
```

The examples below use the default OpenSSL backend.

[SymCrypt]: https://github.com/microsoft/SymCrypt

## Preparing a blob

```bash
cargo run -p tpm_utils --features tpm -- prepare \
    --tpm-version 1.85 \
    --output path/to/vtpm.blob
```

By default this persists an attestation key (AK) and an RSA storage root key
(SRK), and creates an owner-defined AK cert NV index — the shape a
pre-provisioned vTPM normally arrives in.

Useful options:

- `--tpm-version` — `1.38` or `1.85`. Selects the reference implementation, and
  therefore the NVRAM size and VMGS file ID.
- `--ak-cert path/to/cert.der` — write an AK cert into the NV index. Without
  it, the index is created but left uninitialized.
- `--ak-cert-index owner|platform|none` — owner-defined indices mimic an
  externally provisioned vTPM; platform-created ones mimic what OpenHCL
  allocates at boot.
- `--ak-cert-index-size` — index size, in bytes. Defaults to the AK cert size,
  or 4096 when no cert is supplied.
- `--no-ak`, `--no-srk` — skip creating the corresponding key.
- `--mitigation-marker` — create the small-vTPM mitigation marker NV index.
- `--nvram-size` — override the NVRAM region size. Only v1.38 accepts sizes
  other than the one its library was compiled for.

Run `tpm_utils prepare --help` for the full list.

## Writing the blob into a VMGS file

`prepare` prints the matching VmgsTool invocation. For a v1.85 blob:

```bash
cargo run -p vmgstool -- write \
    --file-path path/to/disk.vmgs \
    --file-id TPM_185_NVRAM \
    --data-path path/to/vtpm.blob
```

Booting a VM against that VMGS then exercises the state import path.

```admonish note
The blob size is fixed by the reference implementation the state was prepared
for. Handing a v1.85 vTPM a blob of any other size is rejected at boot, so the
`--tpm-version` used here must match the version the VM is configured with.
```

## Inspecting a blob

`inspect` loads an existing blob and reports the persistent objects and NV
indices it contains, including each index's size, ownership, and whether it has
been written:

```bash
cargo run -p tpm_utils --features tpm -- inspect \
    --tpm-version 1.85 \
    path/to/vtpm.blob
```
