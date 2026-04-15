// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::KdfError;
use openssl_kdf::kdf::Kbkdf;

pub fn kbkdf_hmac_sha256(
    key: &[u8],
    context: &[u8],
    salt: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, KdfError> {
    let mut kdf = Kbkdf::new(
        openssl::hash::MessageDigest::sha256(),
        salt.to_vec(),
        key.to_vec(),
    );
    kdf.set_context(context.to_vec());
    let mut output = vec![0u8; output_len];
    openssl_kdf::kdf::derive(kdf, &mut output).map_err(KdfError)?;
    Ok(output)
}
