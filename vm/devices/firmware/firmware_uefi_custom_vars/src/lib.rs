// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and methods for defining and layering sets of UEFI nvram variables.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use guid::Guid;
use mesh_protobuf::Protobuf;
use thiserror::Error;
use uefi_specs::uefi::nvram::vars::EFI_GLOBAL_VARIABLE;

pub mod delta;

/// A complete base template deferred as raw JSON.
#[derive(Debug, Clone, Protobuf)]
#[mesh(transparent)]
pub struct BaseTemplateJson(Vec<u8>);

impl BaseTemplateJson {
    /// Return the JSON bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for BaseTemplateJson {
    fn from(json: Vec<u8>) -> Self {
        Self(json)
    }
}

/// A customer-provided UEFI variable delta deferred as raw JSON.
#[derive(Debug, Clone, Protobuf)]
#[mesh(transparent)]
pub struct UefiVarsDeltaJson(Vec<u8>);

impl UefiVarsDeltaJson {
    /// Return the JSON bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for UefiVarsDeltaJson {
    fn from(json: Vec<u8>) -> Self {
        Self(json)
    }
}

/// Collection of UEFI nvram variables that will be injected on first boot.
#[derive(Debug, Default, Clone, Protobuf)]
pub struct UefiVars {
    /// Secure Boot signature vars
    pub signatures: Option<Signatures>,
    /// UEFI vars that are not Secure Boot signature vars
    pub non_signature_vars: Vec<(String, UefiVar)>,
}

/// A complete set of variables supplied by a built-in template.
#[derive(Debug, Default, Clone, Protobuf)]
#[mesh(transparent)]
pub struct BaseTemplateVars(UefiVars);

/// A complete set of variables ready for NVRAM injection.
#[derive(Debug)]
pub struct FinalVars(UefiVars);

impl From<UefiVars> for BaseTemplateVars {
    fn from(vars: UefiVars) -> Self {
        Self(vars)
    }
}

impl FinalVars {
    /// Resolve an optional base template and custom delta into final variables.
    pub fn resolve(
        base_template: Option<BaseTemplateVars>,
        custom_template_delta: Option<delta::UefiVarsDelta>,
    ) -> Result<Self, ApplyDeltaError> {
        let base_vars = base_template.map_or_else(UefiVars::default, |base| base.0);
        let final_vars = match custom_template_delta {
            Some(delta) => base_vars.apply_delta(delta)?,
            None => base_vars,
        };
        Ok(Self(final_vars))
    }

    /// Return the finalized UEFI variables.
    pub fn into_uefi_vars(self) -> UefiVars {
        self.0
    }
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
pub struct UefiVar {
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
    RestrictedUefiVar { name: String, guid: Guid },
}

impl UefiVars {
    /// Create a new, blank set of UEFI variables.
    pub fn new() -> UefiVars {
        UefiVars::default()
    }

    /// Apply a delta on top of an existing set of UEFI variables.
    pub fn apply_delta(self, delta: delta::UefiVarsDelta) -> Result<UefiVars, ApplyDeltaError> {
        use delta::SignatureDelta;
        use delta::SignatureDeltaVec;
        use delta::SignaturesAppend;
        use delta::SignaturesDelta;
        use delta::SignaturesReplace;

        let signatures = match (self.signatures, delta.signatures) {
            (None, SignaturesDelta::Append(..)) => {
                return Err(ApplyDeltaError::AppendWithoutBase);
            }
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

        let mut non_signature_vars = self.non_signature_vars;

        // Replace overwritten vars, append new vars
        'outer: for (new_key, new_val) in delta.non_signature_vars {
            if new_key == "dbDefault" && new_val.guid == EFI_GLOBAL_VARIABLE {
                return Err(ApplyDeltaError::RestrictedUefiVar {
                    name: new_key,
                    guid: new_val.guid,
                });
            }

            for (old_key, old_val) in &mut non_signature_vars {
                if *old_key == new_key {
                    *old_val = new_val;
                    continue 'outer;
                }
            }
            non_signature_vars.push((new_key, new_val));
        }

        Ok(UefiVars {
            signatures: Some(signatures),
            non_signature_vars,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::SignatureDelta;
    use crate::delta::SignatureDeltaVec;
    use crate::delta::SignaturesAppend;
    use crate::delta::SignaturesDelta;
    use crate::delta::SignaturesReplace;
    use crate::delta::UefiVarsDelta;

    #[test]
    fn no_templates_resolve_to_empty_final_vars() {
        let vars = FinalVars::resolve(None, None).unwrap().into_uefi_vars();

        assert!(vars.signatures.is_none());
        assert!(vars.non_signature_vars.is_empty());
    }

    #[test]
    fn append_without_base_fails() {
        let delta = UefiVarsDelta {
            signatures: SignaturesDelta::Append(SignaturesAppend {
                kek: None,
                db: None,
                dbx: None,
                moklist: None,
                moklistx: None,
            }),
            non_signature_vars: Vec::new(),
        };

        assert!(matches!(
            FinalVars::resolve(None, Some(delta)),
            Err(ApplyDeltaError::AppendWithoutBase)
        ));
    }

    #[test]
    fn default_without_base_fails() {
        let delta = UefiVarsDelta {
            signatures: SignaturesDelta::Replace(SignaturesReplace {
                pk: SignatureDelta::Default,
                kek: SignatureDeltaVec::Default,
                db: SignatureDeltaVec::Default,
                dbx: SignatureDeltaVec::Default,
                moklist: None,
                moklistx: None,
            }),
            non_signature_vars: Vec::new(),
        };

        assert!(matches!(
            FinalVars::resolve(None, Some(delta)),
            Err(ApplyDeltaError::DefaultWithoutBase)
        ));
    }

    #[test]
    fn restricted_uefi_var_fails() {
        let base_template = UefiVars {
            signatures: Some(Signatures {
                pk: Signature::X509(Vec::new()),
                kek: Vec::new(),
                db: Vec::new(),
                dbx: Vec::new(),
                moklist: Vec::new(),
                moklistx: Vec::new(),
            }),
            non_signature_vars: Vec::new(),
        }
        .into();
        let delta = UefiVarsDelta {
            signatures: SignaturesDelta::Append(SignaturesAppend {
                kek: None,
                db: None,
                dbx: None,
                moklist: None,
                moklistx: None,
            }),
            non_signature_vars: vec![(
                "dbDefault".into(),
                UefiVar {
                    guid: EFI_GLOBAL_VARIABLE,
                    attr: 0,
                    value: Vec::new(),
                },
            )],
        };

        assert!(matches!(
            FinalVars::resolve(Some(base_template), Some(delta)),
            Err(ApplyDeltaError::RestrictedUefiVar { name, guid })
                if name == "dbDefault" && guid == EFI_GLOBAL_VARIABLE
        ));
    }
}
