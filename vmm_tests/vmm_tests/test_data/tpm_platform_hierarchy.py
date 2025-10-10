# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# Simple test to verify TPM platform hierarchy is disabled for guest access

import struct

# TPM2_Clear with platform hierarchy - simplest test case
# Command format: tag(2) + size(4) + command_code(4) + auth_handle(4)
tpm_clear_platform = (
    b'\x80\x01'          # TPM_ST_NO_SESSIONS
    b'\x00\x00\x00\x0E'  # Command size (14 bytes)
    b'\x00\x00\x01\x26'  # TPM_CC_CLEAR
    b'\x40\x00\x00\x0C'  # TPM_RH_PLATFORM
)

with open('/dev/tpmrm0', 'r+b', buffering=0) as tpm:
    tpm.write(tpm_clear_platform)
    response = tpm.read()
    
    # Parse response code from bytes 6-9
    if len(response) >= 10:
        response_code = struct.unpack('>I', response[6:10])[0]
        
        # 0x0085 = TPM_RC_HIERARCHY (hierarchy disabled)
        if response_code == 0x0085:
            print('succeeded')
        else:
            print(f'failed - unexpected response: 0x{response_code:08X}')
    else:
        print('failed - invalid response length')
