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

Legacy response acceptance remains subject to the
[cached-context requirement](attestation.md#stateful-context-lifecycle);
advertising extended-payload support does not require an envelope on an
unbound VM. A malformed flagged envelope is never retried as a legacy body.

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
| 4-5 | Reserved |
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
including framing. AK certificates use a 4 KiB buffer. Envelope parsing allocates
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
