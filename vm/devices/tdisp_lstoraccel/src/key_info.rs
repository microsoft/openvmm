// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reading and writing the key information section, the first section of the
//! DEVICE_SPECIFIC_INFO field reported by a local storage accelerator.

use anyhow::Context;
use zerocopy::FromBytes;

/// Size of the SHA-384 digests reported in this section, which uses the same
/// hash the device negotiated as its SPDM MeasurementHashAlgo.
pub const SHA384_DIGEST_SIZE: usize = 48;

/// MEK present flag value reported when the device holds no key.
const MEK_NOT_LOADED: u8 = 0;

/// MEK present flag value reported when the device holds a key.
const MEK_LOADED: u8 = 1;

/// The state of the device's media encryption key.
///
/// The two forms correspond to the two reports the key provisioning flow reads.
/// Before a key is provisioned the device reports the wrapping key it has
/// generated, so that a key can be wrapped for it. Once one is provisioned the
/// device reports a digest binding the key it was given to the certificate that
/// key was wrapped under, which is what lets us confirm the device holds
/// the key we believe it holds.
///
/// Both forms begin at offset 1, immediately after the present flag.
///
/// The certificate and confirmation digest borrow from the report buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyInfo<'a> {
    /// The device holds no key, and reports the wrapping key to use when
    /// provisioning one.
    NotLoaded {
        /// X509 certificate carrying the wrapping key, with the length prefix
        /// already consumed.
        mek_wrapping_key_cert: &'a [u8],
    },

    /// The device holds a key, and reports its confirmation of that key.
    Loaded {
        /// SHA-384 over the CSP X509 certificate and the wrapped MEK.
        mek_confirmation_digest: &'a [u8; SHA384_DIGEST_SIZE],
    },
}

/// Reads the key information section from the start of the DEVICE_SPECIFIC_INFO
/// field of a local storage accelerator's TDI interface report.
///
/// Returns the section along with the bytes that follow it, which are the NVMe
/// controller section and are not examined here.
///
/// * `data` - The whole DEVICE_SPECIFIC_INFO field, as reported by the device.
pub fn deserialize_key_info(data: &[u8]) -> anyhow::Result<(KeyInfo<'_>, &[u8])> {
    let (mek_present, trailing_bytes) = u8::read_from_prefix(data)
        .map_err(|e| anyhow::anyhow!("failed to deserialize MEK present flag: {e:?}"))?;

    match mek_present {
        MEK_NOT_LOADED => {
            let (wrapping_key_size, trailing_bytes) = u16::read_from_prefix(trailing_bytes)
                .map_err(|e| {
                    anyhow::anyhow!("failed to deserialize MEK wrapping key size: {e:?}")
                })?;

            let wrapping_key_size = wrapping_key_size as usize;
            let (mek_wrapping_key_cert, trailing_bytes) = trailing_bytes
                .split_at_checked(wrapping_key_size)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "key information section claims a {wrapping_key_size} byte wrapping key certificate, but only {} bytes follow",
                        trailing_bytes.len()
                    )
                })?;

            Ok((
                KeyInfo::NotLoaded {
                    mek_wrapping_key_cert,
                },
                trailing_bytes,
            ))
        }
        MEK_LOADED => {
            let (mek_confirmation_digest, trailing_bytes) =
                <[u8; SHA384_DIGEST_SIZE]>::ref_from_prefix(trailing_bytes).map_err(|e| {
                    anyhow::anyhow!("failed to deserialize MEK confirmation digest: {e:?}")
                })?;

            Ok((
                KeyInfo::Loaded {
                    mek_confirmation_digest,
                },
                trailing_bytes,
            ))
        }
        _ => Err(anyhow::anyhow!(
            "MEK present flag is {mek_present:#04x}, which has no defined meaning"
        )),
    }
}

/// Writes a key information section out in the form a device reports it, so
/// that an emulated local storage accelerator can describe its key state.
///
/// * `key_info` - The key state to write out. The wrapping key certificate's
///   length is taken from the certificate itself rather than being supplied
///   separately.
///
/// Returns an error if the wrapping key certificate exceeds the 16-bit
/// length field.
pub fn serialize_key_info(key_info: &KeyInfo<'_>) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::new();

    match key_info {
        KeyInfo::NotLoaded {
            mek_wrapping_key_cert,
        } => {
            let wrapping_key_size = u16::try_from(mek_wrapping_key_cert.len())
                .context("wrapping key certificate does not fit the 16-bit size field")?;

            buffer.push(MEK_NOT_LOADED);
            buffer.extend_from_slice(&wrapping_key_size.to_ne_bytes());
            buffer.extend_from_slice(mek_wrapping_key_cert);
        }
        KeyInfo::Loaded {
            mek_confirmation_digest,
        } => {
            buffer.push(MEK_LOADED);
            buffer.extend_from_slice(*mek_confirmation_digest);
        }
    }

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DEVICE_SPECIFIC_INFO field of a device holding no key, with
    /// `trailing` bytes standing in for the NVMe controller section.
    fn not_loaded(wrapping_key_size: usize, trailing: usize) -> Vec<u8> {
        let certificate = vec![0xee; wrapping_key_size];
        let mut buffer = serialize_key_info(&KeyInfo::NotLoaded {
            mek_wrapping_key_cert: &certificate,
        })
        .unwrap();
        buffer.extend(std::iter::repeat_n(0x11, trailing));
        buffer
    }

    /// The DEVICE_SPECIFIC_INFO field of a device holding a key, with
    /// `trailing` bytes standing in for the NVMe controller section.
    fn loaded(trailing: usize) -> Vec<u8> {
        let mut buffer = serialize_key_info(&KeyInfo::Loaded {
            mek_confirmation_digest: &[0x5a; SHA384_DIGEST_SIZE],
        })
        .unwrap();
        buffer.extend(std::iter::repeat_n(0x22, trailing));
        buffer
    }

    #[test]
    fn wrapping_key_survives_a_round_trip() {
        for len in [0, 600, usize::from(u16::MAX)] {
            let certificate = vec![0xee; len];
            let key_info = KeyInfo::NotLoaded {
                mek_wrapping_key_cert: &certificate,
            };
            let buffer = serialize_key_info(&key_info).unwrap();
            assert_eq!(buffer.len(), 3 + len);
            let (parsed, nvme_section) = deserialize_key_info(&buffer).unwrap();

            assert_eq!(parsed, key_info);
            assert!(nvme_section.is_empty());
        }
    }

    #[test]
    fn wrapping_key_certificate_borrows_the_report_buffer() {
        let buffer = not_loaded(16, 0);
        let (key_info, _) = deserialize_key_info(&buffer).unwrap();
        let KeyInfo::NotLoaded {
            mek_wrapping_key_cert,
        } = key_info
        else {
            panic!("expected a wrapping-key certificate");
        };

        let certificate_offset = size_of::<u8>() + size_of::<u16>();
        assert_eq!(
            mek_wrapping_key_cert.as_ptr(),
            buffer[certificate_offset..].as_ptr()
        );
    }

    #[test]
    fn oversized_wrapping_key_certificate_cannot_be_serialized() {
        let certificate = vec![0xee; usize::from(u16::MAX) + 1];
        let key_info = KeyInfo::NotLoaded {
            mek_wrapping_key_cert: &certificate,
        };

        assert!(serialize_key_info(&key_info).is_err());
    }

    #[test]
    fn confirmation_digest_survives_a_round_trip() {
        let key_info = KeyInfo::Loaded {
            mek_confirmation_digest: &[0x5a; SHA384_DIGEST_SIZE],
        };
        let buffer = serialize_key_info(&key_info).unwrap();
        let (parsed, nvme_section) = deserialize_key_info(&buffer).unwrap();

        assert_eq!(parsed, key_info);
        assert!(nvme_section.is_empty());
    }

    #[test]
    fn confirmation_digest_borrows_the_report_buffer() {
        let buffer = loaded(0);
        let (key_info, _) = deserialize_key_info(&buffer).unwrap();
        let KeyInfo::Loaded {
            mek_confirmation_digest,
        } = key_info
        else {
            panic!("expected a confirmation digest");
        };

        assert_eq!(
            mek_confirmation_digest.as_ptr(),
            buffer[size_of::<u8>()..].as_ptr()
        );
    }

    /// Nothing in the layout pads the wrapping key certificate, so the NVMe
    /// controller section begins wherever the certificate leaves off. Whatever
    /// reads that section must tolerate any offset, and this pins the
    /// arithmetic it will rely on.
    #[test]
    fn trailing_nvme_section_is_returned_unchanged_at_the_expected_offset() {
        for wrapping_key_size in [0, 1, 2, 3, 5, 7, 11, 600, 1023, 4096] {
            let buffer = not_loaded(wrapping_key_size, 128);
            let (_, nvme_section) = deserialize_key_info(&buffer).unwrap();

            assert_eq!(buffer.len() - nvme_section.len(), 3 + wrapping_key_size);
            assert_eq!(nvme_section, [0x11; 128]);
        }

        let buffer = loaded(128);
        let (_, nvme_section) = deserialize_key_info(&buffer).unwrap();
        assert_eq!(buffer.len() - nvme_section.len(), 1 + SHA384_DIGEST_SIZE);
        assert_eq!(nvme_section, [0x22; 128]);
    }

    #[test]
    fn empty_device_specific_info_is_rejected() {
        assert!(deserialize_key_info(&[]).is_err());
    }

    #[test]
    fn undefined_mek_present_flag_is_rejected() {
        for mek_present in [2_u8, 0x7f, 0xff] {
            let mut buffer = vec![mek_present];
            buffer.extend_from_slice(&[0; SHA384_DIGEST_SIZE]);
            assert!(deserialize_key_info(&buffer).is_err());
        }
    }

    #[test]
    fn truncation_before_the_wrapping_key_size_is_rejected() {
        let buffer = not_loaded(16, 0);
        for len in 1..3 {
            assert!(deserialize_key_info(&buffer[..len]).is_err());
        }
    }

    /// Reject both partial and absent certificates when the length claims more
    /// bytes than the field contains.
    #[test]
    fn overlong_wrapping_key_size_is_rejected() {
        for (actual_size, claimed_size) in [(16, 4096_u16), (0, u16::MAX)] {
            let mut buffer = not_loaded(actual_size, 0);
            buffer[1..3].copy_from_slice(&claimed_size.to_ne_bytes());
            assert!(deserialize_key_info(&buffer).is_err());
        }
    }

    #[test]
    fn truncated_confirmation_digest_is_rejected() {
        let buffer = loaded(0);
        for len in 1..buffer.len() {
            assert!(deserialize_key_info(&buffer[..len]).is_err());
        }
    }
}
