# IGVM Attestation Protocol

This page specifies the AK certificate request claims and response contract
used by OpenHCL over the Guest Emulation Transport (GET).

```admonish note title="See also"
[Attestation and VMGS Protection](attestation.md) introduces the attestation
flows. [AK Certificate Renewal](ak_cert.md) describes NV storage, renewal
scheduling, and the trust boundary.
```

## AK request and response versions

AK certificate requests use request-data version V2 and accept V1 or V2
responses. The parser selects the layout from the received response version;
other response versions are rejected.

Guest-requested attestation reports retain the V1 request layout exposed
through TPM NV storage. They do not include the renewal metadata below.

## Existing-certificate claims

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

## AK response framing

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

- [`openhcl_attestation_protocol::igvm_attest::get`][get-rustdoc] defines the
  request and response structures, signal bitmap, and runtime claims.
- [`underhill_attestation`][attestation-rustdoc] implements claim hashing,
  certificate metadata extraction, and response validation.

[get-rustdoc]:
  https://openvmm.dev/rustdoc/openhcl_attestation_protocol/igvm_attest/get/
[attestation-rustdoc]:
  https://openvmm.dev/rustdoc/underhill_attestation/index.html
