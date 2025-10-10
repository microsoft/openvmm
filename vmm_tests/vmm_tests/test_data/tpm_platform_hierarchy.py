# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# Simple test to verify TPM platform hierarchy is disabled for guest access

import struct

# TPM Response Codes
TPM_RC_SUCCESS = 0x0000
TPM_RC_HIERARCHY = 0x0085           # Hierarchy is disabled/not available
TPM_RC_AUTH_FAIL = 0x008E           # Authorization failure
TPM_RC_COMMAND_CODE = 0x0143        # Command not allowed

# TPM error code masks and structure (TPM 2.0 spec)
TPM_RC_FORMAT_ONE_MASK = 0x0080     # Format-One response codes have this bit set
TPM_RC_P = 0x0100                   # Parameter error bit
TPM_RC_1 = 0x0001                   # Parameter 1 (our platform hierarchy handle)
TPM_RC_H = 0x0000                   # Handle error (bits 8-10 = 000)
TPM_RC_S = 0x0800                   # Session error (bits 8-10 = 100)

# Expected error patterns for platform hierarchy being disabled  
# 0x0085 | 0x0080 | 0x0100 | 0x0001 = 0x0185 (hierarchy error + format-one + parameter error + param 1)
TPM_RC_HIERARCHY_P1 = TPM_RC_HIERARCHY | TPM_RC_FORMAT_ONE_MASK | TPM_RC_P | TPM_RC_1  # 0x0185 expected

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
        
        # Check for specific platform hierarchy related errors
        expected_errors = [
            TPM_RC_HIERARCHY,        # Direct hierarchy disabled error
            TPM_RC_HIERARCHY_P1,     # Hierarchy error for parameter 1 (0x0185)
            TPM_RC_AUTH_FAIL,        # Authorization failure
            TPM_RC_COMMAND_CODE      # Command not allowed
        ]
        
        if response_code in expected_errors:
            print('succeeded')
        else:
            print(f'failed - unexpected response: 0x{response_code:08X}')
    else:
        print('failed - invalid response length')
