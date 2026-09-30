# IGVM Attestation Protocol

This page describes IGVM attestation request versions, response framing,
envelope validation, and key-release context encoding.

OpenHCL sends `IGVM_ATTEST` requests through the Guest Emulation Transport
(GET) to the host IGVM agent.

```admonish note title="See also"
[Attestation and VMGS Protection](attestation.md) describes the boot
lifecycle, trust boundary, context adoption, and hardware protector formats.
```

## Request and response versions

`KEY_RELEASE_REQUEST` and `WRAPPED_KEY_REQUEST` advertise V3 in
`IgvmAttestRequestData.version`. The outer request header remains version 2;
V3 retains the V2 request-data extension and capability bitmap. There is no
additional envelope-negotiation capability bit.

| Request | Current request-data version | Accepted response formats |
| --- | --- | --- |
| Key release | V3 | V1/V2 original payload; V3 envelope |
| Wrapped key | V3 | V1/V2 original payload; V3 envelope |
| AK certificate | V2 | V1/V2 binary certificate only |

Since V2, host agents accept newer request versions and return a response
version they support. A V2-only agent therefore returns V2 for a V3 key
request. OpenHCL selects the parser from the actual response version, not
the advertised request version. This works on both provisioning and later
encrypted boots; V2 responses have no envelope or context hash.

Legacy response acceptance remains subject to the
[cached-context requirement](attestation.md#stateful-context-lifecycle);
advertising V3 does not require a V3 response on an unbound VM.

AK certificate requests are explicitly pinned to V2. V3 is not an AK
certificate protocol; its responses are rejected, rather than interpreted as
an envelope or certificate.

## V3 response framing

V3 retains the complete 32-byte V2 binary response header. A UTF-8 JSON
envelope follows it. The offsets below are byte offsets from the response
start; integer header fields are 32-bit little-endian values.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | `data_size`, including header and envelope |
| 4 | 4 | Response `version` = 3 |
| 8 | 4 | `IgvmErrorInfo.error_code` |
| 12 | 4 | `IgvmErrorInfo.http_status_code` |
| 16 | 4 | `IgvmErrorInfo.igvm_signal` bitmap |
| 20 | 12 | `IgvmErrorInfo.reserved` |
| 32 | Variable | JSON envelope |

The parser bounds `data_size` against the supplied response and requires at
least the version-specific header size. It interprets the body only through
`data_size`, not trailing buffer bytes. A nonzero `error_code` remains a
service failure with the existing retry/recovery signals; such an error can
have just the binary header, without a success envelope.

### Errors and signals

The signal bits remain in the binary header. V3 does not move errors or
signals into JSON.

| Bit | Signal |
| --- | --- |
| 0 | Retry |
| 1 | Skip hardware unsealing |
| 2 | RSA-AES key wrap with SHA-384 used |
| 3 | CoRIM endorsement requested |

### Envelope schema

All four top-level fields are required:

| Field | Contract |
| --- | --- |
| `schema_version` | Integer 1; independent of binary response version 3 |
| `request_type` | `key_release` or `wrapped_key`, matching the request |
| `payload` | UTF-8 string containing the original service JSON or JWT |
| `extensions` | Object; `{}` is valid when no metadata is supplied |

`payload` contains the original service response as a JSON string. JSON
escaping preserves the original string after unescaping. Key release uses
the JSON/JWT parser; wrapped key uses the provisioning-service JSON parser.
The envelope is removed once, not recursively. Do not parse and reserialize
signed payload content.

For example, this is an illustrative envelope body, not a complete usable
key-release response (the payload and digest are synthetic):

```json
{
  "schema_version": 1,
  "request_type": "key_release",
  "payload": "{\"ciphertext\":\"<WRAPPED_KEY>\"}",
  "extensions": {
    "key_release_context_hash":
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
  }
}
```

Unknown optional fields in the envelope or extensions are ignored. There is
no `critical` extension mechanism. Duplicate recognized fields, missing
required fields, wrong types, unsupported schema versions, and mismatched
request types are rejected. The envelope and `extensions` must be JSON
objects, not arrays or `null`.

Extensions are interpreted by response type. Only Key Release recognizes
`key_release_context_hash`. Wrapped Key recognizes no extensions:
it ignores all fields, including any field with that name, without checking
the value's type or hash encoding. Duplicate unknown extension names are
also ignored. The surrounding JSON must still be syntactically valid and
within the response limits. Mock agents emit empty Wrapped Key extensions.

### Context hash encoding

In a Key Release response, the optional hash must be a string of exactly
64 ASCII hexadecimal characters encoding 32 bytes. Uppercase, lowercase,
and mixed-case hex are accepted and normalized to lowercase for the hardware
KDF and generated runtime claims. No `0x` prefix, whitespace, or separators
are allowed. Empty, non-hex, incorrectly sized, or non-string values are
rejected. Explicit `null` is invalid; absence means `None`. This rule applies
to the response extension, not to generic deserialization of
`AttestationVmConfig`. Report, request, and JWK fields use their respective
base64 encodings independently of the context hash.

## Limits and failure handling

Key-release and wrapped-key responses use a 64 KiB GET response buffer,
including framing. AK certificates use a 4 KiB buffer. V3 parsing allocates
an owned string for the unescaped payload. JSON escaping and envelope
metadata count toward the response limit, so a service payload that fits
without an envelope may exceed the limit when enveloped. Oversized responses
are rejected.

Malformed framing, invalid UTF-8, invalid envelopes, malformed Key Release
context hashes, and missing required hashes fail closed. They are not reparsed
as legacy responses and do not trigger hardware fallback. These protocol/context
failures are distinct from explicit service errors and transport outages,
which retain the existing retry and hardware-recovery behavior. Inner
service-payload or unwrap failures do not adopt a new hash and retain their
existing handling.

## Implementation references

- [`openhcl_attestation_protocol::igvm_attest::get`][get-rustdoc]
  defines framing, envelope fields, runtime claims, and hash encoding.
- [`underhill_attestation`][attestation-rustdoc] implements response parsing
  and secure key release.

[get-rustdoc]:
  https://openvmm.dev/rustdoc/openhcl_attestation_protocol/igvm_attest/get/
[attestation-rustdoc]:
  https://openvmm.dev/rustdoc/underhill_attestation/index.html
