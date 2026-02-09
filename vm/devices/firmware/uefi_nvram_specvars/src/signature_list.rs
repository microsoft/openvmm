// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to parse, manipulate, and emit [`EFI_SIGNATURE_LIST`] structures.
//!
//! [`ParseSignatureLists`] is the entrypoint to zero-copy iterate over a list
//! of serialized `EFI_SIGNATURE_LIST` objects.

use guid::Guid;
use std::borrow::Cow;
use std::collections::BTreeSet;
use thiserror::Error;
use uefi_specs::uefi::nvram::signature_list::EFI_CERT_SHA256_GUID;
use uefi_specs::uefi::nvram::signature_list::EFI_CERT_X509_GUID;
use uefi_specs::uefi::nvram::signature_list::EFI_SIGNATURE_DATA;
use uefi_specs::uefi::nvram::signature_list::EFI_SIGNATURE_LIST;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct X509Data<'a>(pub Cow<'a, [u8]>);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sha256Data<'a>(pub Cow<'a, [u8; 32]>);

// used as part of the `ParseSignatureLists::collect_signature_lists`
pub enum SignatureDataPayload<'a> {
    X509(&'a [u8]),
    Sha256(&'a [u8; 32]),
}

/// Rust-y representation of a [`EFI_SIGNATURE_DATA`] struct.
#[derive(Debug, PartialEq, Eq)]
pub struct SignatureData<T> {
    pub header: EFI_SIGNATURE_DATA,
    pub data: T,
}

impl SignatureData<Sha256Data<'_>> {
    /// Instantiate a new `SignatureData` with a sha256 digest payload.
    pub fn new_sha256(owner: Guid, data: Cow<'_, [u8; 32]>) -> SignatureData<Sha256Data<'_>> {
        SignatureData {
            header: EFI_SIGNATURE_DATA {
                signature_owner: owner,
            },
            data: Sha256Data(data),
        }
    }
}

impl SignatureData<X509Data<'_>> {
    /// Instantiate a new `SignatureData` with a x509 cert payload.
    pub fn new_x509(owner: Guid, data: Cow<'_, [u8]>) -> SignatureData<X509Data<'_>> {
        SignatureData {
            header: EFI_SIGNATURE_DATA {
                signature_owner: owner,
            },
            data: X509Data(data),
        }
    }
}

impl SignatureData<Sha256Data<'_>> {
    fn extend_as_spec_signature_data(&self, v: &mut Vec<u8>) {
        v.extend(self.header.as_bytes());
        v.extend(self.data.0.as_bytes());
    }
}

impl SignatureData<X509Data<'_>> {
    fn extend_as_spec_signature_data(&self, v: &mut Vec<u8>) {
        v.extend(self.header.as_bytes());
        v.extend(self.data.0.as_bytes());
    }
}

/// Rust-y representation of a [`EFI_SIGNATURE_LIST`] struct.
#[derive(Debug, PartialEq, Eq)]
pub enum SignatureList<'a> {
    Sha256(Vec<SignatureData<Sha256Data<'a>>>),
    // assume that each signature list only contains a single cert
    //
    // While the spec _technically_ allows stuffing multiple certs into a single
    // signature list, the only way that could occur is if the certs have
    // exactly the same length, which never actually happens in practice.
    X509(SignatureData<X509Data<'a>>),
}

impl SignatureList<'_> {
    /// Serialize the signature list as a `EFI_SIGNATURE_LIST` into a vec
    pub fn extend_as_spec_signature_list(&self, res: &mut Vec<u8>) {
        let (signature_type, sig_data_size, multiplier) = match &self {
            SignatureList::Sha256(sigs) => (EFI_CERT_SHA256_GUID, 32, sigs.len()),
            SignatureList::X509(sig) => (EFI_CERT_X509_GUID, sig.data.0.len(), 1),
        };

        let signature_size = size_of::<EFI_SIGNATURE_DATA>() + sig_data_size;

        let header = EFI_SIGNATURE_LIST {
            signature_type,
            signature_list_size: (size_of::<EFI_SIGNATURE_LIST>() + (signature_size * multiplier))
                as u32,
            signature_header_size: 0, // always zero
            signature_size: signature_size as u32,
        };

        res.extend(header.as_bytes());
        match self {
            SignatureList::Sha256(sigs) => {
                for sig in sigs {
                    sig.extend_as_spec_signature_data(res)
                }
            }
            SignatureList::X509(sig) => sig.extend_as_spec_signature_data(res),
        }
    }
}

/// Errors which may occur during `EFI_SIGNATURE_LIST` parsing.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("could not read signature list header")]
    InvalidHeader,
    #[error("unsupported signature type: {0}")]
    UnsupportedSignatureType(Guid),
    #[error("buffer contains less data than specified in EFI_SIGNATURE_LIST header")]
    TruncatedData,

    #[error("invalid signature_size specified for sha256 (expected 32 + 16, got {0})")]
    Sha256InvalidSigSize(u32),
    #[error("unexpected end of buffer while reading sha256 EFI_SIGNATURE_DATA header")]
    Sha256InvalidHeader,
    #[error("unexpected end of buffer while reading sha256 EFI_SIGNATURE_DATA data")]
    Sha256TruncatedData,

    #[error("invalid signature_size specified for x509 (expected {0}, got {1})")]
    X509InvalidSigSize(u32, u32),
    #[error("unexpected end of buffer while reading x509 EFI_SIGNATURE_DATA header")]
    X509InvalidHeader,
}

/// Iterator over a series of `EFI_SIGNATURE_LIST` structs in a single buffer.
pub struct ParseSignatureLists<'a> {
    buf: &'a [u8],
}

impl<'a> ParseSignatureLists<'a> {
    /// Instantiate a `ParseSignatureLists` with the given `buf`
    pub fn new(buf: &'a [u8]) -> ParseSignatureLists<'a> {
        ParseSignatureLists { buf }
    }

    /// Parse a list of `EFI_SIGNATURE_LIST`s into a
    /// [`Vec<SignatureList>`](SignatureList).
    ///
    /// `filter` can be used to discard certain signatures (e.g: checking for
    /// duplicate signatures alongside [`Self::collect_signature_set`])
    pub fn collect_signature_lists<F>(
        self,
        mut filter: F,
    ) -> Result<Vec<SignatureList<'a>>, ParseError>
    where
        F: FnMut(EFI_SIGNATURE_DATA, SignatureDataPayload<'_>) -> bool,
    {
        let mut lists = Vec::new();

        for list in self {
            let list = list?;
            let list = match list {
                ParseSignatureList::X509(mut certs) => {
                    let cert = certs.next().unwrap()?;
                    assert!(certs.next().is_none());

                    if !filter(cert.header, SignatureDataPayload::X509(&cert.data.0)) {
                        continue;
                    }

                    SignatureList::X509(cert)
                }
                ParseSignatureList::Sha256(sigs) => {
                    let mut list = Vec::new();
                    for sig in sigs {
                        let sig = sig?;

                        if !filter(sig.header, SignatureDataPayload::Sha256(&sig.data.0)) {
                            continue;
                        }

                        list.push(sig);
                    }

                    // if all the signatures were filtered out, don't include an
                    // empty signature list
                    if list.is_empty() {
                        continue;
                    }

                    SignatureList::Sha256(list)
                }
            };
            lists.push(list)
        }

        Ok(lists)
    }

    /// Parse a list of `EFI_SIGNATURE_LIST`s into a [`BTreeSet`] of signatures.
    pub fn collect_signature_set(
        self,
    ) -> Result<BTreeSet<(EFI_SIGNATURE_DATA, Cow<'a, [u8]>)>, ParseError> {
        let mut sig_set = BTreeSet::new();

        for list in self {
            let list = list?;
            match list {
                ParseSignatureList::X509(mut certs) => {
                    let cert = certs.next().unwrap()?;
                    assert!(certs.next().is_none());
                    sig_set.insert((cert.header, cert.data.0));
                }
                ParseSignatureList::Sha256(sigs) => {
                    for sig in sigs {
                        let sig = sig?;
                        sig_set.insert((
                            sig.header,
                            match sig.data.0 {
                                Cow::Borrowed(a) => Cow::Borrowed(a),
                                Cow::Owned(a) => Cow::Owned(a.to_vec()),
                            },
                        ));
                    }
                }
            };
        }

        Ok(sig_set)
    }

    fn next_inner(&mut self) -> Result<Option<ParseSignatureList<'a>>, ParseError> {
        if self.buf.is_empty() {
            return Ok(None);
        }

        let (
            EFI_SIGNATURE_LIST {
                signature_type,
                signature_list_size,
                signature_header_size: _,
                signature_size,
            },
            buf,
        ) = EFI_SIGNATURE_LIST::read_from_prefix(self.buf)
            .map_err(|_| ParseError::InvalidHeader)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let expected_data_len = signature_list_size as usize - size_of::<EFI_SIGNATURE_LIST>();
        if buf.len() < expected_data_len {
            return Err(ParseError::TruncatedData);
        }
        let (data, buf) = buf.split_at(expected_data_len);

        let res = match signature_type {
            EFI_CERT_SHA256_GUID => {
                ParseSignatureList::Sha256(ParseSignatureSha256::new(data, signature_size)?)
            }
            EFI_CERT_X509_GUID => {
                ParseSignatureList::X509(ParseSignatureX509::new(data, signature_size)?)
            }
            sig => return Err(ParseError::UnsupportedSignatureType(sig)),
        };

        self.buf = buf;

        Ok(Some(res))
    }
}

impl<'a> Iterator for ParseSignatureLists<'a> {
    type Item = Result<ParseSignatureList<'a>, ParseError>;

    fn next(&mut self) -> Option<Result<ParseSignatureList<'a>, ParseError>> {
        self.next_inner().transpose()
    }
}

/// Parsers for various kinds of `EFI_SIGNATURE_DATA` lists.
pub enum ParseSignatureList<'a> {
    X509(ParseSignatureX509<'a>),
    Sha256(ParseSignatureSha256<'a>),
}

/// Iterator over `EFI_SIGNATURE_DATA` objects containing x509 certs.
pub struct ParseSignatureX509<'a> {
    buf: &'a [u8],
}

impl<'a> ParseSignatureX509<'a> {
    fn new(buf: &'a [u8], signature_size: u32) -> Result<ParseSignatureX509<'a>, ParseError> {
        if buf.len() != signature_size as usize {
            return Err(ParseError::X509InvalidSigSize(
                signature_size,
                buf.len() as u32,
            ));
        }

        Ok(ParseSignatureX509 { buf })
    }

    // assume there is only single cert per signature list
    //
    // see comment associated with `SignatureList::X509` for rationale
    fn next_inner(&mut self) -> Result<Option<SignatureData<X509Data<'a>>>, ParseError> {
        if self.buf.is_empty() {
            return Ok(None);
        }

        let (header, buf) = EFI_SIGNATURE_DATA::read_from_prefix(self.buf)
            .map_err(|_| ParseError::X509InvalidHeader)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let val: Cow<'a, [u8]> = buf.into();
        let res = SignatureData::new_x509(header.signature_owner, val);

        self.buf = &[];
        Ok(Some(res))
    }
}

impl<'a> Iterator for ParseSignatureX509<'a> {
    type Item = Result<SignatureData<X509Data<'a>>, ParseError>;

    fn next(&mut self) -> Option<Result<SignatureData<X509Data<'a>>, ParseError>> {
        self.next_inner().transpose()
    }
}

/// Iterator over `EFI_SIGNATURE_DATA` objects containing sha256 digests.
pub struct ParseSignatureSha256<'a> {
    buf: &'a [u8],
}

impl<'a> ParseSignatureSha256<'a> {
    fn new(buf: &'a [u8], signature_size: u32) -> Result<ParseSignatureSha256<'a>, ParseError> {
        let expected_signature_size = 32 + size_of::<EFI_SIGNATURE_DATA>();

        if signature_size != expected_signature_size as u32 {
            return Err(ParseError::Sha256InvalidSigSize(signature_size));
        }

        // sha256 has consistent signature sizes, so we can perform some early
        // validation as an optimization
        if !buf.len().is_multiple_of(expected_signature_size) {
            return Err(ParseError::Sha256TruncatedData);
        }

        Ok(ParseSignatureSha256 { buf })
    }

    fn next_inner(&mut self) -> Result<Option<SignatureData<Sha256Data<'a>>>, ParseError> {
        if self.buf.is_empty() {
            return Ok(None);
        }

        let (header, buf) =
            EFI_SIGNATURE_DATA::read_from_prefix(self.buf).expect("buf size validated in `new`"); // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let expected_data_len = 32;
        assert!(buf.len() >= expected_data_len, "validated in new()");
        let (signature, buf) = buf.split_at(expected_data_len);

        let val: &'a [u8; 32] = signature.try_into().unwrap();
        let res = SignatureData::new_sha256(header.signature_owner, Cow::Borrowed(val));

        self.buf = buf;
        Ok(Some(res))
    }
}

impl<'a> Iterator for ParseSignatureSha256<'a> {
    type Item = Result<SignatureData<Sha256Data<'a>>, ParseError>;

    fn next(&mut self) -> Option<Result<SignatureData<Sha256Data<'a>>, ParseError>> {
        self.next_inner().transpose()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const OWNER_1: Guid = Guid {
        data1: 1,
        data2: 0,
        data3: 0,
        data4: [0, 0, 0, 0, 0, 0, 0, 0],
    };

    const OWNER_2: Guid = Guid {
        data1: 2,
        data2: 0,
        data3: 0,
        data4: [0, 0, 0, 0, 0, 0, 0, 0],
    };

    fn test_data() -> Vec<SignatureList<'static>> {
        vec![
            SignatureList::Sha256(vec![
                SignatureData::new_sha256(OWNER_1, Cow::Owned([0; 32])),
                SignatureData::new_sha256(OWNER_2, Cow::Owned([1; 32])),
                SignatureData::new_sha256(OWNER_1, Cow::Owned([2; 32])),
            ]),
            SignatureList::X509(SignatureData::new_x509(
                OWNER_2,
                b"some cert data"[..].into(),
            )),
            SignatureList::Sha256(vec![
                SignatureData::new_sha256(OWNER_1, Cow::Owned([0; 32])),
                SignatureData::new_sha256(OWNER_2, Cow::Owned([1; 32])),
            ]),
            SignatureList::X509(SignatureData::new_x509(
                OWNER_1,
                b"more cert data"[..].into(),
            )),
        ]
    }

    fn test_data_no_owner_1() -> Vec<SignatureList<'static>> {
        vec![
            SignatureList::Sha256(vec![SignatureData::new_sha256(
                OWNER_2,
                Cow::Owned([1; 32]),
            )]),
            SignatureList::X509(SignatureData::new_x509(
                OWNER_2,
                b"some cert data"[..].into(),
            )),
            SignatureList::Sha256(vec![SignatureData::new_sha256(
                OWNER_2,
                Cow::Owned([1; 32]),
            )]),
        ]
    }

    fn dump_to_vec(lists: Vec<SignatureList<'_>>) -> Vec<u8> {
        let mut buf = Vec::new();
        for l in &lists {
            l.extend_as_spec_signature_list(&mut buf)
        }
        buf
    }

    #[test]
    fn roundtrip() {
        let lists = test_data();

        // dump the list of signature lists into a buffer
        let mut buf = Vec::new();
        for l in &lists {
            l.extend_as_spec_signature_list(&mut buf)
        }

        // reconstruct list of signature lists using the parser framework
        let new_lists = ParseSignatureLists::new(&buf)
            .collect_signature_lists(|_, _| true)
            .unwrap();

        assert_eq!(lists, new_lists);
    }

    #[test]
    fn filter() {
        let lists = test_data();
        let lists_no_owner_1 = test_data_no_owner_1();

        let lists_buf = dump_to_vec(lists);
        let new_lists_no_owner_1 = ParseSignatureLists::new(&lists_buf)
            .collect_signature_lists(|header, _| header.signature_owner != OWNER_1)
            .unwrap();

        assert_eq!(lists_no_owner_1, new_lists_no_owner_1);
    }
}
