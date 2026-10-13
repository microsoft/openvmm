// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA key generation and TPM 2.0 import-blob serialization.

use crate::marshal;
use crate::marshal::TpmtSensitive;
use crate::create_sensitive_file;
use crate::print_sha256_hash;
use anyhow::Context;
use crypto::rsa::RsaKeyPair;
use crypto::rsa::RsaPrivateKeyComponents;
use der::Encode;
use std::fs;
use std::io::Write;
use std::path::Path;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::TpmaObjectBits;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmsRsaParams;
use tpm_protocol::tpm20proto::protocol::TpmtPublic;
use tpm_protocol::tpm20proto::protocol::TpmtRsaScheme;
use tpm_protocol::tpm20proto::protocol::TpmtSymDefObject;
use zerocopy::FromZeros;

/// Creates a random RSA key in TPM 2.0 import-blob format.
pub fn create_random_rsa_key_in_tpm2_import_blob_format(
    public_key_file: &str,
    private_key_tpm2b_file: &str,
) -> anyhow::Result<()> {
    let private_key = RsaKeyPair::generate(2048).context("failed to generate RSA key")?;
    let public_components = private_key.to_components();
    let public_key_der = pkcs1::RsaPublicKey {
        modulus: der::asn1::UintRef::new(&public_components.modulus)
            .context("failed to encode RSA modulus")?,
        public_exponent: der::asn1::UintRef::new(&public_components.public_exponent)
            .context("failed to encode RSA public exponent")?,
    }
    .to_der()
    .context("failed to encode RSA public key as PKCS#1 DER")?;
    let private_components = private_key
        .to_private_components()
        .context("failed to export RSA private key components")?;
    tracing::info!("RSA 2048-bit key generated.");

    let import_blob = get_key_in_tpm2_import_format_rsa(&private_components)?;
    write_key_files(
        public_key_file,
        &public_key_der,
        private_key_tpm2b_file,
        &import_blob,
    )?;
    tracing::info!("RSA public key is saved to {public_key_file} in DER PKCS1 format.");
    print_sha256_hash(&public_key_der);

    tracing::info!("RSA private key is saved to {private_key_tpm2b_file} in TPM2B import format.");

    let private_key_der = private_key
        .to_pkcs8_der()
        .context("failed to encode RSA private key as PKCS#8 DER")?;
    print_sha256_hash(&private_key_der);
    Ok(())
}

fn write_key_files(
    public_key_file: &str,
    public_key: &[u8],
    private_key_file: &str,
    private_key: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !Path::new(public_key_file)
            .try_exists()
            .context("failed to check public key destination")?,
        "public key destination already exists"
    );
    anyhow::ensure!(
        !Path::new(private_key_file)
            .try_exists()
            .context("failed to check TPM import blob destination")?,
        "TPM import blob destination already exists"
    );

    let mut public_file =
        create_sensitive_file(public_key_file).context("failed to create public key file")?;
    let mut private_file = match create_sensitive_file(private_key_file) {
        Ok(file) => file,
        Err(error) => {
            drop(public_file);
            let _ = fs::remove_file(public_key_file);
            return Err(error).context("failed to create TPM import blob file");
        }
    };

    public_file
        .write_all(public_key)
        .context("failed to write public key file")?;
    private_file
        .write_all(private_key)
        .context("failed to write TPM import blob file")?;
    Ok(())
}

fn get_key_in_tpm2_import_format_rsa(
    private_key: &RsaPrivateKeyComponents,
) -> anyhow::Result<Vec<u8>> {
    let key_bits = u16::try_from(private_key.modulus.len() * 8)
        .context("RSA modulus is too large for TPM key parameters")?;
    let symmetric_def = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
    let rsa_scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);

    let public_area = TpmtPublic::new(
        AlgIdEnum::RSA.into(),
        AlgIdEnum::SHA256.into(),
        TpmaObjectBits::new()
            .with_user_with_auth(true)
            .with_decrypt(true),
        &[],
        TpmsRsaParams::new(symmetric_def, rsa_scheme, key_bits, 0),
        &private_key.modulus,
    )
    .context("failed to construct TPM RSA public area")?;
    let public = Tpm2bPublic::new(public_area);

    // TPM2B_PRIVATE_KEY_RSA contains one RSA prime factor. The TPM derives the
    // remaining private values from this factor and the public modulus.
    let sensitive = TpmtSensitive {
        sensitive_type: public_area.my_type,
        auth_value: Tpm2bBuffer::new_zeroed(),
        seed_value: Tpm2bBuffer::new_zeroed(),
        sensitive: Tpm2bBuffer::new(&private_key.prime1)
            .context("RSA prime is too large for TPM sensitive data")?,
    };
    let sensitive = marshal::tpmt_sensitive_marshal(&sensitive);

    let mut import_blob = public.serialize();
    import_blob.extend_from_slice(&(sensitive.len() as u16).to_be_bytes());
    import_blob.extend_from_slice(&sensitive);
    import_blob.extend_from_slice(&0_u16.to_be_bytes());
    Ok(import_blob)
}

#[cfg(test)]
mod tests {
    use super::get_key_in_tpm2_import_format_rsa;
    use super::write_key_files;
    use crypto::rsa::RsaKeyPair;
    use std::fs;
    use test_with_tracing::test;
    use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;

    #[test]
    fn import_blob_has_three_length_delimited_fields() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let private_components = key.to_private_components().unwrap();
        let blob = get_key_in_tpm2_import_format_rsa(&private_components).unwrap();

        let public = Tpm2bPublic::deserialize(&blob).unwrap();
        let private_offset = 2 + public.size.get() as usize;
        let private_size =
            u16::from_be_bytes(blob[private_offset..private_offset + 2].try_into().unwrap())
                as usize;
        let encrypted_secret_offset = private_offset + 2 + private_size;

        assert_eq!(&blob[encrypted_secret_offset..], &[0, 0]);
    }

    #[test]
    fn key_files_reject_same_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");

        assert!(
            write_key_files(
                path.to_str().unwrap(),
                b"public",
                path.to_str().unwrap(),
                b"private",
            )
            .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn key_files_do_not_create_public_key_when_private_destination_exists() {
        let directory = tempfile::tempdir().unwrap();
        let public_path = directory.path().join("public");
        let private_path = directory.path().join("private");
        fs::write(&private_path, b"existing private key").unwrap();

        assert!(
            write_key_files(
                public_path.to_str().unwrap(),
                b"public",
                private_path.to_str().unwrap(),
                b"replacement private",
            )
            .is_err()
        );
        assert!(!public_path.exists());
        assert_eq!(fs::read(&private_path).unwrap(), b"existing private key");
    }

    #[test]
    fn key_files_preserve_existing_hard_link_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let private_path = directory.path().join("private");
        let public_path = directory.path().join("public");
        fs::write(&private_path, b"existing private key").unwrap();
        fs::hard_link(&private_path, &public_path).unwrap();

        assert!(
            write_key_files(
                public_path.to_str().unwrap(),
                b"public",
                private_path.to_str().unwrap(),
                b"replacement private",
            )
            .is_err()
        );
        assert_eq!(fs::read(&public_path).unwrap(), b"existing private key");
        assert_eq!(fs::read(&private_path).unwrap(), b"existing private key");
    }
}
