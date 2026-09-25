// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Synthetic fixture only. No hardware calls, private keys, or network traffic.
import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';

// Synthetic digest-sized bytes, not a prescribed context-hash construction.
const contextHash = Buffer.from(Array.from({ length: 32 }, (_, i) => i)).toString('hex');
const claims = {
    keys: [{
        kid: 'HCLTransferKey',
        key_ops: ['encrypt'],
        kty: 'RSA',
        e: Buffer.from([1, 0, 1]).toString('base64url'),
        // Placeholder modulus, NOT a generated RSA key. No private key exists.
        n: Buffer.alloc(256, 0xa5).toString('base64url'),
    }],
    'vm-configuration': {
        'current-time': 1691103220,
        'root-cert-thumbprint': '',
        'console-enabled': false,
        'interactive-console-enabled': false,
        'ipmi-enabled': false,
        'secure-boot': true,
        'tpm-enabled': true,
        'tpm-version': '1.38',
        'tpm-persisted': true,
        'filtered-vpci-devices-allowed': false,
        vmUniqueId: '11111111-2222-3333-4444-555555555555',
        'hardware-sealing-policy': 'none',
        'key-release-context-hash': contextHash,
    },
};
// These exact UTF-8 bytes, not the pretty-printed object, are report-bound.
const claimsJson = JSON.stringify(claims);
const claimsBytes = Buffer.from(claimsJson, 'utf8');
const digest = createHash('sha256').update(claimsBytes).digest();
const report = Buffer.alloc(1184);
report.writeUInt32LE(2, 0); // SNP report version 2.
report.writeUInt32LE(1, 52); // ECDSA P-384/SHA-384 identifier; signature remains zero.
digest.copy(report, 80); // REPORT_DATA: SHA-256 digest followed by 32 zero bytes.

// IgvmAttestRequestBase: 32-byte header, 1184-byte report, 20-byte request data.
// Version 2 adds a 4-byte capability bitmap, then the claims without a NUL.
const request = Buffer.alloc(1240 + claimsBytes.length);
const header = {
    signature: 0x414c4348,
    version: 2,
    report_size: request.length,
    request_type: 1, // KEY_RELEASE_REQUEST
    status: 0,
    reserved: [0, 0, 0],
};
const requestData = {
    data_size: 24 + claimsBytes.length,
    version: 2,
    report_type: 2, // SNP_VM_REPORT
    report_data_hash_type: 1, // SHA_256
    variable_data_size: claimsBytes.length,
};
const headerWords = [header.signature, header.version, header.report_size,
    header.request_type, header.status, ...header.reserved];
headerWords.forEach((word, i) => request.writeUInt32LE(word, i * 4));
report.copy(request, 32);
Object.values(requestData).forEach((word, i) => request.writeUInt32LE(word, 1216 + i * 4));
// error_code, retry, skip_hw_unsealing, use_rsa_aes_key_wrap_384; no TDX CoRIM bit.
request.writeUInt32LE(15, 1236);
claimsBytes.copy(request, 1240);

const fixture = {
    mock_only: true,
    description: 'Synthetic SNP KEY_RELEASE_REQUEST payload; unsigned report and placeholder transfer key. Not valid attestation evidence. Excludes outer GET transport framing.',
    header,
    request_data: requestData,
    request_data_extension: { capability_bitmap: 15 },
    runtime_claims: claims,
    runtime_claims_json: claimsJson,
    runtime_claims_sha256_hex: digest.toString('hex'),
    snp_report: {
        version: 2,
        vmpl: 0,
        signature_algo: 1,
        report_data_hex: report.subarray(80, 144).toString('hex'),
        note: 'All other fields, including the 512-byte signature, are zero placeholders.',
        bytes_base64: report.toString('base64'),
    },
    request_base64: request.toString('base64'),
};
const args = process.argv.slice(2);
const overwrite = args.includes('--force');
const output = args.find(arg => arg !== '--force')
    ?? new URL('./mock_igvmattest_snp_request.json', import.meta.url);
// Regeneration after a schema change must explicitly opt in to replacing files.
writeFileSync(output, `${JSON.stringify(fixture, null, 2)}\n`, { flag: overwrite ? 'w' : 'wx' });
// Keep the default binary fixture byte-for-byte consistent with the JSON.
if (output instanceof URL) {
    writeFileSync(new URL('./mock_igvmattest_snp_request.bin', import.meta.url),
        request, { flag: overwrite ? 'w' : 'wx' });
}