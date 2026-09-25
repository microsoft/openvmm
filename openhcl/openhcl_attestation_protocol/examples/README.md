# Mock IGVM attestation examples

These examples illustrate request bytes and response metadata. For the contract,
see the Guide's
[wire protocol](../../../Guide/src/reference/architecture/openhcl/attestation_protocol.md)
and [attestation lifecycle](../../../Guide/src/reference/architecture/openhcl/attestation.md)
references, including context binding, hardware KDF, and protector formats.

## Complete SNP request

`mock_igvmattest_snp_request.json` contains a complete version-2
`KEY_RELEASE_REQUEST` payload using `SNP_VM_REPORT` and SHA-256:

- `runtime_claims`: readable claims with a placeholder `HCLTransferKey` RSA JWK
	and VM configuration including `key-release-context-hash`.
- `runtime_claims_json`: the exact compact UTF-8 JSON bytes appended to the
	request and hashed into the report. Do not hash the pretty-printed object.
- `snp_report.bytes_base64`: all 1,184 report bytes. `report_data` contains the
	claims' SHA-256 digest followed by 32 zero bytes.
- `request_base64`: the entire binary IGVM attestation payload: 32-byte header,
	SNP report, 20-byte request data, 4-byte capability bitmap, and claims bytes.
	This excludes outer GET framing, shared-memory addresses, and response buffers.

Decode `request_base64` with standard base64 to obtain the binary payload.
Header enum values are numeric: request type 1, report type 2, hash type 1.
The capability bitmap is 15, matching the current SNP request builder.
This fixture remains V2; see the
[wire protocol](../../../Guide/src/reference/architecture/openhcl/attestation_protocol.md)
for current version selection.

This is **not valid attestation evidence**: the SNP signature is all zero,
other report fields are placeholders, and the synthetic RSA modulus is not a
generated key. It cannot support real attestation or key release. No hardware,
network call, or host-policy transport is involved. The context hash is explicitly
supplied to the mock before hashing; a later host response cannot retroactively
bind it into this report.

`generate_mock_snp_request.mjs` generates the fixture deterministically with
Node.js built-ins. By default it writes both JSON and binary fixtures. It accepts
an optional JSON output path and requires `--force` to replace existing files.
The Rust `test_mock_snp_request_matches_wire_format` test checks
the claims binding and compares the encoded request against `create_request`.

## Response metadata fragment

`mock_igvmattest_response.json` is an illustrative host response fragment with
a synthetic `key_release_context_hash` value. The fixture represents the 32
bytes `00` through `1f` as hex:

```text
000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
```

This is a synthetic hash-sized value, not the digest of an actual key name
and key-release policy hash.

This fragment is not a complete V3 IGVM_ATTEST response or a valid key-release
response. It has no binary response header, four-field envelope, wrapped key,
JWT, or signature and cannot be passed to the response parser. Runtime
mock-agent tests generate complete V3 responses with wrapped test keys.
