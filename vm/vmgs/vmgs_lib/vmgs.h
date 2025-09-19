// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C"
{
#endif

enum VmgsError
{
    VmgsOk = 0,
    VmgsNullParam = 1,
    VmgsCantOpenFile = 2,
    VmgsCantReadFile = 3,
    VmgsFileDisk = 4,
    VmgsInvalidBufSize = 5,
    VmgsInvalidFileID = 6,
    VmgsInvalidFileSize = 7,
    VmgsInvalidString = 8,
    VmgsInvalidVmgs = 9,
    VmgsFileInfoNotAllocated = 10,
    VmgsDecryptionFailed = 11,
    VmgsEncryptionFailed = 12,
    VmgsWriteFailed = 13,
    VmgsFileExists = 14,
};

enum FileId
{
    FileTable = 0,
    BiosNvram = 1,
    TpmPpi = 2,
    TpmNvram = 3,
    RtcSkew = 4,
    Attest = 5,
    KeyProtector = 6,
    VmUniqueId = 7,
    GuestFirmware = 8,
    CustomUefi = 9,
};

// Read from file_id of file_path
//
// If reading encrypted data, `use_encryption` must be true
// and `encryption_key` must point to a valid null-terminated utf-8 string
//
// `file_path` must point to a valid null-terminated utf-8 string
// `in_len` must be equal to the value from query_size_vmgs
// `in_buf` points to an array of size `in_len`
enum VmgsError read_vmgs(
    char *file_path,
    enum FileId file_id,
    char *encryption_key,
    bool use_encryption,
    char *in_buf,
    int64_t in_len);

// Write data into `file_id` of `file_path`
//
// If writing encrypted data, `use_encryption` must be true
// and `encryption_key` must point to a valid null-terminated utf-8 string
//
// `file_path` and `data_path` must point to valid null-terminated utf-8 strings
enum VmgsError write_vmgs(
    char *file_path,
    char *data_path,
    enum FileId file_id,
    char *encryption_key,
    bool use_encryption);

// Create and initialize `file_path` with `file_size`
//
// If file is to be encrypted, `use_encryption` must be true
// and `encryption_key` must point to a valid null-terminated utf-8 string
//
// `path` must point to a valid null-terminated utf-8 string
// if `file_size` is zero, default file size of 4MB is used
enum VmgsError create_vmgs(
    char *path,
    uint64_t file_size,
    bool force_create,
    char *encryption_key,
    bool use_encryption);

// Queries the size of `file_id` of `file_path`
//
// `file_path` must point to a valid null-terminated utf-8 string
// `out_size` will point to the size necessary to perform a read_vmgs
// call on the same file_id in file_path
enum VmgsError query_size_vmgs(
    char *file_path,
    enum FileId file_id,
    int64_t *out_size);

#ifdef __cplusplus
}
#endif
