# vTPMState Blob

The blob is used for testing TPM with pre-provisioned states. Refer to the following tests for usage examples:

- tpm_device::tests::test_fix_corrupted_vmgs
- tpm_lib::tests::test_with_pre_provisioned_state
- tpm_lib::tests::test_initialize_guest_secret_key

## Steps to Create the Blob

1. Initialize TPM (an `MsTpm20RefPlatform` instance) with callbacks that define the data store where TPM will write its NV states.
2. Configure the TPM with the desired commands to establish the required state.
3. Once the TPM state is prepared, export the data store to generate the blob.
