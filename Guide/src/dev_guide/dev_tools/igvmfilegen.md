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
DLL with one image and one language, the embedded IGVM is selected
automatically. If it contains multiple `VMFW` entries, supply
`--resource-id <RESOURCE_ID>`. If that entry has multiple languages, also supply
`--resource-language <LANGUAGE_ID>`. Missing or ambiguous selections fail rather
than choosing the first image. These selectors also apply to `dump-corim` and
the finalized input of `verify-endorsement`.

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
policy. A file that already has an ID block is rejected unless you explicitly
request [replacement](#prepare-and-re-sign-an-existing-snp-id-block).

If an SNP CoRIM already exists, adding or replacing the ID block requires its
SVN to match the CoRIM's exact guest SVN. Missing, malformed, or ambiguous SVN
claims are rejected rather than ignored.

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
`.idblock` payload emitted by `manifest` using ECDSA P-384 with SHA-384, then
attach the payload, DER-encoded signature, and signer public key or certificate
(SPKI or X.509, PEM or DER):

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

## Prepare and re-sign an existing SNP ID block

For an existing IGVM image, `prepare-snp-id-block` validates its identity,
layout, policy, and launch measurement, then exports the exact 96-byte firmware
signing payload. It does not rebuild the image or substitute identity defaults:

```bash
cargo run -p igvmfilegen -- prepare-snp-id-block \
  --input path/to/original.igvm \
  --output path/to/snp.idblock \
  --dump
```

The optional `--dump` prints decoded JSON to stdout: `ld` (launch digest),
`family_id`, and `image_id` are hex strings; `version` and `guest_svn` are
numbers; `policy` is a hexadecimal string. The binary output is unchanged.
Sign the binary payload, not the JSON. Omitting `--dump` exports only the
binary payload; neither form establishes signer trust.

This command requires either a valid existing ID signature or a canonical
unsigned placeholder. The placeholder retains the measured launch digest and
identity, uses version 1, and has zero ID-key algorithm, signature, and
public-key fields. Reserved and unused author-key fields must also be zero;
author-key signing is unsupported. An invalid signature, partially cleared
authentication structure, or missing ID block is not a placeholder. These
authentication checks are independent of CoRIM preparation.

Have the authorized ID signer sign the exported bytes using ECDSA P-384 with
SHA-384, then apply its DER-encoded signature:

```bash
cargo run -p igvmfilegen -- add-snp-id-block \
  --input path/to/original.igvm \
  --output path/to/id-signed.igvm \
  --replace-existing \
  --id-block path/to/snp.idblock \
  --id-signature path/to/id-signature.der \
  --id-public-key path/to/id-signer.pem
```

Replacement requires the payload to match the original identity, policy, SVN,
and launch digest byte for byte. Only ID-key authentication fields change;
temporary-key replacement is not allowed. Changing the ID key can change
sealing identity without changing measurement. ID signature verification does
not establish that the signing key is authorized.

## Prepare a CoRIM endorsement for an existing image

`prepare-endorsement` prepares a launch-measurement CoRIM from an existing raw
IGVM without rebuilding it. Select one platform with `--platform snp|tdx|vbs`.
The CoRIM conditionally endorses the **exact guest SVN**, not a minimum:

- **SNP with an ID block:** derive the SVN from the measurement/policy-bound
  block; reject a conflicting `--guest-svn` override. A malformed or ambiguous
  block is an error, not a reason to fall back to an explicit SVN.
- **SNP without an ID block, TDX, or VBS:** require an explicit `--guest-svn`.

CoRIM preparation does **not** authenticate the ID signature, change ID-block
bytes, or emit ID signing inputs. If ID signing is needed, complete it first
and use that image as the preparation and verification baseline. To retain an
authorized existing ID signature, skip re-signing and validate its signature
and signer authorization separately.

For example, prepare an endorsement for the ID-signed SNP image above:

```bash
cargo run -p igvmfilegen -- prepare-endorsement \
  --input path/to/id-signed.igvm \
  --output-dir path/to/signing-inputs \
  --platform snp
```

For TDX, VBS, or SNP without an ID block, supply the SVN explicitly:

```bash
cargo run -p igvmfilegen -- prepare-endorsement \
  --input path/to/tdx.igvm \
  --output-dir path/to/tdx-signing-inputs \
  --platform tdx --guest-svn <GUEST_SVN>
```

The selected platform must not already contain CoRIM data. The output directory
must not exist, and its parent must exist. Preparation publishes:

- `image.igvm`: the image with the unsigned CoRIM document embedded.
- `corim.cbor`: the exact embedded unsigned CoRIM bytes to endorse.
- `endorsement-inputs.json`: the schema v2 completion marker and provenance.

The manifest records input and tool SHA-256 hashes, platform, compatibility
mask, launch digest, SVN, and each artifact's size and hash. Files are published
individually, with the manifest published last; the directory is **not** an
atomic transaction. Require the marker and verify all listed hashes before
requesting signatures. Retry failed or interrupted preparation in a new
directory, and keep completed inputs immutable. The record is not signed
provenance: obtain it through a trusted pipeline. It records
`release_ready: false`, with ID authentication and signer trust not checked.

## Patch a CoRIM signature

`manifest` or `prepare-endorsement` writes the CoRIM document into the IGVM.
Have the authorized endorsement signer sign the exact document using the
supported PS384 COSE envelope format. Attach the signed bundle or detached
signature for one platform. For the existing-image workflow above:

```bash
cargo run -p igvmfilegen -- patch-corim-signature \
  --input path/to/signing-inputs/image.igvm \
  --output path/to/finalized.igvm \
  --platform snp \
  --corim-bundle path/to/signed-corim.cose
```

Use `--corim-signature` instead when the input is already a detached COSE_Sign1
signature. For a manifest-generated image, use the manifest output as `--input`
instead. A bundle's document must exactly match the embedded document.

The tool verifies a well-formed PS384 envelope and verifies its signature over
the IGVM-embedded document using the key carried in `x5chain` or `x5bag`.

```admonish warning title="Signature trust"
This verification checks signature integrity and algorithm compatibility. It
does not establish certificate-chain trust, check revocation, or enforce
certificate policy. The caller must obtain the signature from a trusted signing
pipeline and validate that pipeline's identity separately.
```

## Verify an existing-image endorsement

After `prepare-endorsement` and signature patching, verify the finalized image
against the immutable image used for preparation. If you re-signed an SNP ID
block, this baseline is the **ID-signed original**, not the image from before
ID signing:

```bash
cargo run -p igvmfilegen -- verify-endorsement \
  --original path/to/id-signed.igvm \
  --input path/to/finalized.igvm \
  --platform snp > path/to/verification.json
```

For TDX, VBS, or SNP without an ID block, repeat the explicit `--guest-svn`
used during preparation. Verification checks the selected CoRIM signature and
exact expected CoRIM additions, rejecting other changes. All ID-block bytes,
including authentication, must remain unchanged; verification does **not**
validate the ID signature or signer. The schema v2 JSON receipt binds the
original and finalized IGVM hashes and records these trust limits; it is not
a signed authorization statement. Packaging should rerun verification against
its actual inputs rather than trusting receipt contents alone.

## Firmware resource and packaging boundary

Preparation and ID signing operate on raw IGVM files. A producer deferring ID
signing through `prepare-snp-id-block` must supply a canonical placeholder,
not omit the block. Producer configuration and signing transport belong to
the producing pipeline.

Select the finalized IGVM as the firmware resource input **before** resource
compilation and DLL signing. Changing an IGVM after the DLL is built does not
update its embedded resource. Final package signing and consumer/hardware
acceptance remain separate release gates.

`verify-endorsement` also accepts a firmware DLL as `--input`; `--original`
remains a raw IGVM. For a DLL with multiple firmware entries, select the
resource explicitly:

```bash
cargo run -p igvmfilegen -- verify-endorsement \
  --original path/to/id-signed.igvm \
  --input path/to/firmware.dll \
  --resource-id <RESOURCE_ID> \
  --platform snp
```

If the resource has multiple languages, also supply
`--resource-language <LANGUAGE_ID>`. The receipt hashes the extracted IGVM
payload, not the DLL container; DLL signature validation is a separate gate.

## Troubleshooting

- A missing resource names the symbolic manifest entry that could not be
  resolved; check the resources JSON and the referenced build output.
- A platform-policy error usually indicates incompatible manifest directives,
  guest policy, architecture, or isolation type.
- A measurement mismatch should be investigated as a layout or input change,
  not bypassed.
- ID-block errors can indicate a signing payload from a different image, an
  unsupported key, an existing block without `--replace-existing`, or an SVN
  mismatch with an embedded SNP CoRIM.
- CoRIM errors distinguish document mismatch, unsupported algorithms, malformed
  COSE, and failed signature math. None of these establish signer trust.
