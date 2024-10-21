// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and methods for defining and layering sets of custom UEFI nvram
//! variables

#![forbid(unsafe_code)]

use guid::Guid;
use mesh_protobuf::Protobuf;
use thiserror::Error;
use uefi_specs::uefi::nvram::vars::EFI_GLOBAL_VARIABLE;

pub mod delta;

/// Collection of UEFI nvram variables that that will be injected on first boot.
#[derive(Debug, Default, Clone, Protobuf)]
pub struct CustomVars {
    /// Secure Boot signature vars
    pub signatures: Option<Signatures>,
    /// Any additional custom vars
    pub custom_vars: Vec<(String, CustomVar)>,
}

#[derive(Debug, Clone, Protobuf)]
pub struct Signatures {
    pub pk: Signature,
    pub kek: Vec<Signature>,
    pub db: Vec<Signature>,
    pub dbx: Vec<Signature>,
    pub moklist: Vec<Signature>,
    pub moklistx: Vec<Signature>,
}

#[derive(Debug, Clone, Protobuf)]
pub enum Signature {
    X509(Vec<X509Cert>),
    Sha256(Vec<Sha256Digest>),
}

#[derive(Debug, Clone, Protobuf)]
pub struct CustomVar {
    pub guid: Guid,
    pub attr: u32,
    pub value: Vec<u8>,
}

#[derive(Clone, Protobuf)]
pub struct X509Cert(pub Vec<u8>);

impl std::fmt::Debug for X509Cert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("X509Cert").field(&"[..]").finish()
    }
}

#[derive(Clone, Protobuf)]
pub struct Sha256Digest(pub [u8; 32]);

impl std::fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sha256Digest")
            .field(&self.0.map(|b| format!("{:02x?}", b)).join(""))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ApplyDeltaError {
    #[error("cannot Append if no base signatures are provided")]
    AppendWithoutBase,
    #[error("cannot use \"Default\" variable type if no base signatures are provided")]
    DefaultWithoutBase,
    #[error("cannot set restricted variable: {name}:{guid}")]
    RestrictedCustomVar { name: String, guid: Guid },
}

impl CustomVars {
    /// Create a new, blank set of CustomVars.
    pub fn new() -> CustomVars {
        CustomVars::default()
    }

    /// Apply a delta on-top of an existing set of CustomVars.
    pub fn apply_delta(self, delta: delta::CustomVarsDelta) -> Result<CustomVars, ApplyDeltaError> {
        use delta::SignatureDelta;
        use delta::SignatureDeltaVec;
        use delta::SignaturesAppend;
        use delta::SignaturesDelta;
        use delta::SignaturesReplace;

        let signatures = match (self.signatures, delta.signatures) {
            (None, SignaturesDelta::Append(..)) => return Err(ApplyDeltaError::AppendWithoutBase),
            (
                None,
                SignaturesDelta::Replace(SignaturesReplace {
                    pk,
                    kek,
                    db,
                    dbx,
                    moklist,
                    moklistx,
                }),
            ) => {
                fn deny_default(sig_delta: SignatureDelta) -> Result<Signature, ApplyDeltaError> {
                    match sig_delta {
                        SignatureDelta::Sig(sig) => Ok(sig),
                        SignatureDelta::Default => Err(ApplyDeltaError::DefaultWithoutBase),
                    }
                }

                fn deny_default_vec(
                    sig_delta_vec: SignatureDeltaVec,
                ) -> Result<Vec<Signature>, ApplyDeltaError> {
                    match sig_delta_vec {
                        SignatureDeltaVec::Sigs(sig) => Ok(sig),
                        SignatureDeltaVec::Default => Err(ApplyDeltaError::DefaultWithoutBase),
                    }
                }

                Signatures {
                    pk: deny_default(pk)?,
                    kek: deny_default_vec(kek)?,
                    db: deny_default_vec(db)?,
                    dbx: deny_default_vec(dbx)?,
                    moklist: moklist
                        .map(deny_default_vec)
                        .transpose()?
                        .unwrap_or_default(),
                    moklistx: moklistx
                        .map(deny_default_vec)
                        .transpose()?
                        .unwrap_or_default(),
                }
            }
            (
                Some(Signatures {
                    pk,
                    mut kek,
                    mut db,
                    mut dbx,
                    mut moklist,
                    mut moklistx,
                }),
                sig_delta,
            ) => match sig_delta {
                SignaturesDelta::Append(SignaturesAppend {
                    kek: append_kek,
                    db: append_db,
                    dbx: append_dbx,
                    moklist: append_moklist,
                    moklistx: append_moklistx,
                }) => {
                    if let Some(append_kek) = append_kek {
                        kek.extend(append_kek);
                    }

                    if let Some(append_db) = append_db {
                        db.extend(append_db);
                    }

                    if let Some(append_dbx) = append_dbx {
                        dbx.extend(append_dbx);
                    }

                    if let Some(append_moklist) = append_moklist {
                        moklist.extend(append_moklist)
                    }

                    if let Some(append_moklistx) = append_moklistx {
                        moklistx.extend(append_moklistx)
                    }

                    Signatures {
                        pk,
                        kek,
                        db,
                        dbx,
                        moklist,
                        moklistx,
                    }
                }
                SignaturesDelta::Replace(SignaturesReplace {
                    pk: replace_pk,
                    kek: replace_kek,
                    db: replace_db,
                    dbx: replace_dbx,
                    moklist: replace_moklist,
                    moklistx: replace_moklistx,
                }) => {
                    fn replace_default(sig_delta: SignatureDelta, base: Signature) -> Signature {
                        match sig_delta {
                            SignatureDelta::Sig(sig) => sig,
                            SignatureDelta::Default => base,
                        }
                    }

                    fn replace_default_vec(
                        sig_delta_vec: SignatureDeltaVec,
                        base: Vec<Signature>,
                    ) -> Vec<Signature> {
                        match sig_delta_vec {
                            SignatureDeltaVec::Sigs(sigs) => sigs,
                            SignatureDeltaVec::Default => base,
                        }
                    }

                    fn replace_default_option_vec(
                        sig_delta_vec: Option<SignatureDeltaVec>,
                        base: Vec<Signature>,
                    ) -> Vec<Signature> {
                        match sig_delta_vec {
                            Some(SignatureDeltaVec::Sigs(sigs)) => sigs,
                            Some(SignatureDeltaVec::Default) | None => base,
                        }
                    }

                    Signatures {
                        pk: replace_default(replace_pk, pk),
                        kek: replace_default_vec(replace_kek, kek),
                        db: replace_default_vec(replace_db, db),
                        dbx: replace_default_vec(replace_dbx, dbx),
                        moklist: replace_default_option_vec(replace_moklist, moklist),
                        moklistx: replace_default_option_vec(replace_moklistx, moklistx),
                    }
                }
            },
        };

        let mut custom_vars = self.custom_vars;

        // Replace overwritten vars, append new vars
        'outer: for (new_key, new_val) in delta.custom_vars {
            if new_key.as_str() == "dbDefault" && new_val.guid == EFI_GLOBAL_VARIABLE {
                return Err(ApplyDeltaError::RestrictedCustomVar {
                    name: new_key,
                    guid: new_val.guid,
                });
            }

            for (old_key, old_val) in &mut custom_vars {
                if *old_key == new_key {
                    *old_val = new_val;
                    continue 'outer;
                }
            }
            custom_vars.push((new_key, new_val));
        }

        Ok(CustomVars {
            signatures: Some(signatures),
            custom_vars,
        })
    }
}
