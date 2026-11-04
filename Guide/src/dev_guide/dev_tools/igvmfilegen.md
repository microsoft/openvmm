# igvmfilegen

`igvmfilegen` constructs, measures, inspects, and signs metadata for Isolated
Guest Virtual Machine (IGVM) files.

## Role in an OpenHCL build

An OpenHCL IGVM combines several independently built inputs, including the boot
loader, Linux kernel, initrd, `openvmm_hcl`, optional sidecar, and guest boot
resources. A manifest describes how to place those resources and which VBS,
SEV-SNP, or Intel TDX platform definitions the file supports.

`igvmfilegen` turns that description into the final binary layout and computes
the platform launch measurements and endorsement material associated with it.

Use `cargo xflowey build-igvm <RECIPE>` for normal OpenHCL builds. Flowey
selects a supported manifest and produces the resource file from concrete build
artifacts. Invoke `igvmfilegen` directly when developing manifests, inspecting
an image, or completing a signing workflow.

## Build

Build the host utility with:

```bash
cargo build -p igvmfilegen
```

Display the current top-level interface with:

```bash
cargo run -p igvmfilegen -- --help
```

## Build from a manifest

The `manifest` command consumes two JSON files:

- The manifest describes images, platform policies, memory layout, command
  lines, and the relationship between components.
- The resources file maps symbolic manifest resources to concrete files.

Build an image with:

```bash
cargo run -p igvmfilegen -- manifest \
  --manifest path/to/manifest.json \
  --resources path/to/resources.json \
  --output path/to/openhcl.bin
```

`--debug-validation` enables additional build-time checks.
`--disable-secure-avic` overrides secure AVIC for supported debug SNP
configurations.

```admonish danger
`--confidential-debug` changes the measured OpenHCL command line so a
confidential guest permits diagnostics and trusts host-provided debug options.
Use it only for an intentionally debuggable development image. It weakens the
security assumptions expected from a production confidential image.
```

For each measurable platform, the command also writes sibling identity and
endorsement files next to the output:

```text
<BASE>-snp.json     <BASE>-snp.cbor
<BASE>-tdx.json     <BASE>-tdx.cbor
<BASE>-vbs.json     <BASE>-vbs.cbor
<BASE>-snp.idblock
```

Only files corresponding to platforms present in the manifest are emitted.
The `.idblock` file is the SNP signing payload used by the production signing
mode described below.

## Inspect an IGVM

Dump headers and directives in a human-readable form:

```bash
cargo run -p igvmfilegen -- dump \
  --filepath path/to/openhcl.bin
```

The input may be a raw IGVM or a supported `vmfirmwareigvm` resource DLL. For a
DLL, the embedded IGVM is selected automatically.

Inspect CoRIM entries and optionally extract their payloads:

```bash
cargo run -p igvmfilegen -- dump-corim \
  --filepath path/to/openhcl.bin \
  --platform snp \
  --output path/to/corim-output
```

`--header-type document|signature` narrows the result. Without filters, the
command reports every supported CoRIM entry in the file.

## Add an SNP ID block

An SNP ID block binds an identity key and guest SVN to the image launch
measurement. The IGVM must already contain a compatible SNP platform and guest
policy, and a file that already has an ID block is rejected.

For development, generate an ephemeral P-384 key and specify an SVN:

```bash
cargo run -p igvmfilegen -- add-snp-id-block \
  --input path/to/input.bin \
  --output path/to/output.bin \
  --guest-svn <GUEST_SVN>
```

Alternatively, `--manifest path/to/manifest.json` sources the guest SVN and
image identity from the manifest's SNP configuration.

Production signing keeps the private key outside this process. Sign the exact
`.idblock` payload emitted by `manifest`, then attach the payload, DER ECDSA
signature, and signer public key or certificate:

```bash
cargo run -p igvmfilegen -- add-snp-id-block \
  --input path/to/input.bin \
  --output path/to/output.bin \
  --id-block path/to/image-snp.idblock \
  --id-signature path/to/signature.der \
  --id-public-key path/to/public-key.pem
```

The output path may equal the input path, but a separate output is easier to
recover and compare during development.

## Patch a CoRIM signature

`manifest` writes the CoRIM document into the IGVM. After an external signer
produces a signed bundle or detached signature, attach it for one platform:

```bash
cargo run -p igvmfilegen -- patch-corim-signature \
  --input path/to/input.bin \
  --output path/to/output.bin \
  --platform snp \
  --corim-bundle path/to/signed-corim.cose
```

Use `--corim-signature` instead when the input is already a detached COSE_Sign1
signature.

The tool verifies a well-formed PS384 envelope and verifies its signature over
the IGVM-embedded document using the key carried in `x5chain` or `x5bag`.

```admonish warning title="Signature trust"
This verification checks signature integrity and algorithm compatibility. It
does not establish certificate-chain trust, check revocation, or enforce
certificate policy. The caller must obtain the signature from a trusted signing
pipeline and validate that pipeline's identity separately.
```

## Troubleshooting

- A missing resource names the symbolic manifest entry that could not be
  resolved; check the resources JSON and the referenced build output.
- A platform-policy error usually indicates incompatible manifest directives,
  guest policy, architecture, or isolation type.
- A measurement mismatch should be investigated as a layout or input change,
  not bypassed.
- ID-block errors can indicate a signing payload from a different image, an
  unsupported key, or an IGVM that already contains a block.
- CoRIM errors distinguish document mismatch, unsupported algorithms, malformed
  COSE, and failed signature math. None of these establish signer trust.
