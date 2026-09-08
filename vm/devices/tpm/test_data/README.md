# vTPM State Blobs

These blobs hold pre-provisioned vTPM NVRAM state and are used for testing.
Refer to the following tests for usage examples:

- tpm_device::tests::test_fix_corrupted_vmgs
- tpm_lib::tests::test_with_pre_provisioned_state
- tpm_lib::tests::test_initialize_guest_secret_key

| Blob | TPM version | Origin |
| --- | --- | --- |
| `vTpmState.blob` | 1.38 | The TpmEngFWInit (internal) tool |
| `vTpmState-corrupt.blob` | 1.38 | `vTpmState.blob`, corrupted by hand |
| `vTpmState-1.85.blob` | 1.85 | `tpm_utils prepare` |

## Regenerating `vTpmState-1.85.blob`

The blob holds an AK, an SRK, and a 1024-byte owner-defined AK cert index:

```sh
head -c 1024 /dev/zero | tr '\0' '\006' > akcert.bin
cargo run -p tpm_utils --features tpm -- prepare \
    --tpm-version 1.85 \
    --ak-cert akcert.bin \
    --ak-cert-index owner \
    -o vm/devices/tpm/test_data/vTpmState-1.85.blob
```

Use `tpm_utils inspect` to see what a blob contains.
