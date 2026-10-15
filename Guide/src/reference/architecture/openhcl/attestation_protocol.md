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

`KEY_RELEASE_REQUEST` and `WRAPPED_KEY_REQUEST` use V2 in
`IgvmAttestRequestData.version` and set
`IGVM_REQUEST_EXTENDED_PAYLOAD_BIT` (`1u << 5`) in the request capability
bitmap. The outer request header and request-data extension remain unchanged.
No new binary protocol version is introduced.

| Request | Current request-data version | Accepted response formats |
| --- | --- | --- |
| Key release | V2, extended-payload bit set | V1/V2 original payload; V2 flagged envelope |
| Wrapped key | V2, extended-payload bit set | V1/V2 original payload; V2 flagged envelope |
| AK certificate | V2 | V1/V2 binary certificate only |

An agent sets `IGVM_SIGNAL_EXTENDED_PAYLOAD_BIT` (`1u << 6`) in the V2
response signal bitmap when it returns an envelope. OpenHCL selects the
body parser from that response bit, not the request capability or body shape.
If the bit is clear, the body retains its original service format and
supplies no context hash. V1 responses also retain their legacy format.

Advertising extended-payload support does not require an envelope, even
when an earlier hardware protector supplied a cached context hint. Missing
context is an ordinary compatibility case for the
[agent callout optimization](attestation.md#agent-callout-optimization),
not an authorization failure. A malformed flagged envelope is never
retried as a legacy body.

AK certificate requests remain V2 and do not set the extended-payload bit.
Flagged AK certificate responses are rejected. Unknown header versions,
including version 3, are rejected for all request types.

## Extended response framing

An extended response retains the complete 32-byte V2 header. A UTF-8 JSON
envelope follows it. The offsets below are byte offsets from the response
start; integer header fields are 32-bit little-endian values.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | `data_size`, including header and envelope |
| 4 | 4 | Response `version` = 2 |
| 8 | 4 | `IgvmErrorInfo.error_code` |
| 12 | 4 | `IgvmErrorInfo.http_status_code` |
| 16 | 4 | `IgvmErrorInfo.igvm_signal`, bit 6 set |
| 20 | 12 | `IgvmErrorInfo.reserved` |
| 32 | Variable | JSON envelope |

The parser bounds `data_size` against the supplied response and requires at
least the version-specific header size. It interprets the body only through
`data_size`, not trailing buffer bytes. A nonzero `error_code` remains a
service failure with the existing retry/recovery signals; such an error can
have just the binary header, without a success envelope. Error handling
takes precedence over the extended-payload bit.

### Errors and signals

The signal bits remain in the binary header. Extended payloads do not move
errors or signals into JSON.

| Bit | Signal |
| --- | --- |
| 0 | Retry |
| 1 | Skip hardware unsealing |
| 2 | RSA-AES key wrap with SHA-384 used |
| 3 | CoRIM endorsement requested |
| 4 | Reserved |
| 5 | AK certificate no-renewal (`IGVM_SIGNAL_NO_RENEW_CERT_BIT`) |
| 6 | Extended JSON payload (`IGVM_SIGNAL_EXTENDED_PAYLOAD_BIT`) |

### Envelope schema

All four top-level fields are required:

| Field | Contract |
| --- | --- |
| `schema_version` | Integer 1; independent of binary response version 2 |
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
no `critical` extension mechanism. Duplicate recognized top-level fields,
missing required fields, wrong types for required fields, unsupported schema
versions, and mismatched request types are rejected. The envelope and
`extensions` must be JSON objects, not arrays or `null`.

Extensions are interpreted by response type. Only Key Release recognizes
`key_release_context_hash`. Wrapped Key recognizes no extensions:
it ignores all fields, including any field with that name, without checking
the value's type or hash encoding. Duplicate unknown extension names are
also ignored. The surrounding JSON must still be syntactically valid and
within the response limits. Mock agents emit empty Wrapped Key extensions.

### Context hash encoding

Context is agent-provided optimization metadata, not an authorization or
freshness assertion. In a Key Release response, the optional hash must be a
string of exactly
64 ASCII hexadecimal characters encoding 32 bytes. Uppercase, lowercase,
and mixed-case hex are accepted and normalized to lowercase for the hardware
KDF and generated runtime claims. No `0x` prefix, whitespace, or separators
are allowed in an accepted hash. Missing, empty, non-hex, incorrectly sized,
non-string, or explicit `null` values are treated as absent (`None`). Duplicate
hash fields discard the entire hash regardless of their order or values.
Discarded metadata produces a rate-limited warning containing only a safe
reason, never the supplied value; a missing field does not produce a warning.
This sanitization applies only to the optional response extension, not to
cached configuration or stored protector validation. Report, request, and JWK
fields use their respective base64 encodings independently of the context hash.

## Limits and failure handling

Key-release and wrapped-key responses use a 64 KiB GET response buffer,
including framing. AK certificates use a 4 KiB buffer. Envelope parsing allocates
an owned string for the unescaped payload. JSON escaping and envelope
metadata count toward the response limit, so a service payload that fits
without an envelope may exceed the limit when enveloped. Oversized responses
are rejected.

Malformed framing, invalid UTF-8, and invalid envelopes are rejected. They
are not reparsed as legacy responses and do not trigger hardware fallback.
These protocol
failures are distinct from explicit service errors and transport outages,
which retain the existing retry and hardware-recovery behavior. Inner
service-payload or unwrap failures do not adopt a new hash and retain their
existing handling. An omitted or discarded optional hash does not prevent
successful SKR or hardware sealing: a fresh hardware protector uses V3 without
context binding. Existing V4 ingress protectors still require their recorded
context for authenticated unsealing. An accepted hash is adopted only after
successful key unwrap and VMGS initialization; hardware persistence is separate
from its use in this boot.

## AK request and response versions

AK certificate requests use request-data version V2 and accept V1 or V2
responses. Other response versions and the extended-payload response signal
are rejected. Guest-requested attestation reports retain the V1 request
layout exposed through TPM NV storage; they omit renewal metadata.

### Existing-certificate claims

An AK request with successfully decoded existing-certificate metadata includes
an optional top-level `akcert` object in its runtime claims. If metadata is
unavailable, the entire object is omitted, not serialized as `null`.
Key-release requests and guest-requested reports omit it.

| Field | Encoding |
| --- | --- |
| `issuer` | Certificate issuer common name, not its full distinguished name. |
| `subject` | Certificate subject common name. |
| `not_before` | ASN.1 time content: `YYMMDDhhmmssZ` for UTCTime or `YYYYMMDDhhmmssZ` for GeneralizedTime. |
| `not_after` | The same encoding rule, selected by this field's ASN.1 time type. |
| `authority` | Uppercase hex of the Authority Key Identifier's `keyIdentifier` bytes; empty string if absent. |

The field names use underscores exactly as shown. The time strings are not
Unix timestamps or RFC 3339 strings. The authority identifier has no prefix or
separators and does not encode the complete extension.

For example, this is an illustrative claim object, not a usable certificate:

```json
{
  "akcert": {
    "issuer": "Example AK issuer",
    "subject": "Example AK",
    "not_before": "260101000000Z",
    "not_after": "270101000000Z",
    "authority": "0123456789ABCDEF"
  }
}
```

Claims are serialized with JSON escaping before hashing. The SHA-256 digest
occupies the first 32 bytes of the 64-byte report-data field; the remaining
bytes are zero. Adding `akcert` therefore changes the report binding, not just
an unmeasured transport field.

### AK response framing

Header integers are 32-bit little-endian values. `data_size` includes the
header and any payload.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | `data_size` |
| 4 | 4 | Response `version` |
| 8 | 4 | V2 only: `IgvmErrorInfo.error_code` |
| 12 | 4 | V2 only: `IgvmErrorInfo.http_status_code` |
| 16 | 4 | V2 only: `IgvmErrorInfo.igvm_signal` |
| 20 | 12 | V2 only: reserved |

V1 has an 8-byte header; V2 has a 32-byte header. Certificate bytes follow
the version-specific header directly, without a JSON response envelope.
The AK response buffer is 4 KiB, including the header.

The parser requires a complete version-specific header within `data_size`,
rejects a declared size larger than the supplied buffer, and ignores bytes
after the declared size. A nonzero V2 `error_code` is an error, even when
the no-renewal signal is also set.

### No-renewal signal

V2 signal **bit 5** (`IGVM_SIGNAL_NO_RENEW_CERT_BIT`, mask `0x20`) means that
the service recommends retaining the existing AK certificate described in the
request. Bit 4 remains reserved.

A successful, validly framed response with bit 5 set returns a no-renewal
outcome instead of certificate bytes. The signal takes precedence over any
payload in that response. Other signal bits alone do not indicate no-renewal.
V1 has no signal field and cannot express this outcome.

The TPM caller additionally requires that the request included successfully
decoded certificate metadata before accepting no-renewal. See
[renewal outcomes](ak_cert.md#renewal-outcomes) for timer, NV, and failure
handling. An empty response or certificate payload is distinct from this
explicit signal.

## Implementation references

- [`openhcl_attestation_protocol::igvm_attest::get`][get-rustdoc]
  defines framing, envelope fields, runtime claims, and hash encoding.
- [`underhill_attestation`][attestation-rustdoc] implements response parsing
  and secure key release.

[get-rustdoc]:
  https://openvmm.dev/rustdoc/openhcl_attestation_protocol/igvm_attest/get/
[attestation-rustdoc]:
  https://openvmm.dev/rustdoc/underhill_attestation/index.html
