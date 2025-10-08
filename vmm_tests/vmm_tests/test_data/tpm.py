# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# The test script that reads AK CERT from TPM and validate its content

# TPM NV READ command for Index 0x01c101d0 (AK CERT) that takes size (u32) and offset (u32) arguments through appending
command = b'\x80\x02\x00\x00\x00\x23\x00\x00\x01\x4e\x40\x00\x00\x01\x01\xc1\x01\xd0\x00\x00\x00\x09\x40\x00\x00\x09\x00\x00\x00\x00\x00'
expected_output = bytearray([0xab] * 2500  + [0x00] * 1596)

output = b''
with open('/dev/tpmrm0', 'r+b', buffering=0) as tpm:
    # NV READ can read up to 1024 bytes at a time while AK CERT INDEX is 4096 bytes
    size = 1024
    offset = 0
    for i in range(4):
        tpm.write(command + size.to_bytes(2, 'big') + offset.to_bytes(2, 'big'))
        response = tpm.read()
        # Extract the payload
        output += response[16:len(response) - 5]
        offset += size

try:
    nonzero_size = output.index(0)
    print(f"output (size full {len(output)}, nonzero {nonzero_size}): {output}")
except ValueError:
    print(f"output (size full {len(output)}): {output}")

if output == expected_output:
    print('succeeded')
else:
    print('failed')
