// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data types which define a "delta" operation on a
//! [`CustomVars`](super::CustomVars) struct.

use super::CustomVar;
use super::Signature;
use mesh_protobuf::Protobuf;

/// How custom Secure Boot signature variables modify a selected base template.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Protobuf)]
pub enum SecureBootCustomization {
    /// Custom entries are appended to the base template.
    Append,
    /// Some base template variables are retained and others are replaced.
    PartialReplace,
    /// PK, KEK, db, and dbx are all explicitly replaced.
    FullReplace,
}

impl SecureBootCustomization {
    /// Return the stable telemetry name for this customization mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::PartialReplace => "partial-replace",
            Self::FullReplace => "full-replace",
        }
    }
}

/// Collection of custom UEFI nvram variables.
#[derive(Debug)]
pub struct CustomVarsDelta {
    /// Secure Boot signature vars
    pub signatures: SignaturesDelta,
    /// Any additional custom vars
    pub custom_vars: Vec<(String, CustomVar)>,
}

impl CustomVarsDelta {
    /// Classify how this delta modifies Secure Boot signature variables.
    pub fn secure_boot_customization(&self) -> SecureBootCustomization {
        match &self.signatures {
            SignaturesDelta::Append(_) => SecureBootCustomization::Append,
            SignaturesDelta::Replace(signatures)
                if matches!(signatures.pk, SignatureDelta::Sig(_))
                    && matches!(signatures.kek, SignatureDeltaVec::Sigs(_))
                    && matches!(signatures.db, SignatureDeltaVec::Sigs(_))
                    && matches!(signatures.dbx, SignatureDeltaVec::Sigs(_)) =>
            {
                SecureBootCustomization::FullReplace
            }
            SignaturesDelta::Replace(_) => SecureBootCustomization::PartialReplace,
        }
    }
}

#[derive(Debug)]
pub enum SignaturesDelta {
    /// Vars should append onto underlying template
    Append(SignaturesAppend),
    /// Vars should replace the underlying template
    Replace(SignaturesReplace),
}

/// Append CANNOT be used with `pk`
#[derive(Debug, Clone)]
pub struct SignaturesAppend {
    pub kek: Option<Vec<Signature>>,
    pub db: Option<Vec<Signature>>,
    pub dbx: Option<Vec<Signature>>,
    pub moklist: Option<Vec<Signature>>,
    pub moklistx: Option<Vec<Signature>>,
}

/// Replace MUST include the base secure boot vars, and may optionally include
/// the moklist vars.
#[derive(Debug, Clone)]
pub struct SignaturesReplace {
    pub pk: SignatureDelta,
    pub kek: SignatureDeltaVec,
    pub db: SignatureDeltaVec,
    pub dbx: SignatureDeltaVec,
    pub moklist: Option<SignatureDeltaVec>,
    pub moklistx: Option<SignatureDeltaVec>,
}

#[derive(Debug, Clone)]
pub enum SignatureDelta {
    Sig(Signature),
    /// "Default" will pull the value of the signature from the specified
    /// hardcoded template (and fail if one wasn't specified)
    ///
    /// It shouldn't be used in the hardcoded templates
    Default,
}

#[derive(Debug, Clone)]
pub enum SignatureDeltaVec {
    Sigs(Vec<Signature>),
    /// "Default" will pull the value of the signature from the specified
    /// hardcoded template (and fail if one wasn't specified)
    ///
    /// It shouldn't be used in the hardcoded templates
    Default,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature() -> Signature {
        Signature::Sha256(Vec::new())
    }

    #[test]
    fn classifies_secure_boot_customization() {
        let append = CustomVarsDelta {
            signatures: SignaturesDelta::Append(SignaturesAppend {
                kek: None,
                db: None,
                dbx: None,
                moklist: None,
                moklistx: None,
            }),
            custom_vars: Vec::new(),
        };
        assert_eq!(
            append.secure_boot_customization(),
            SecureBootCustomization::Append
        );

        let partial_replace = CustomVarsDelta {
            signatures: SignaturesDelta::Replace(SignaturesReplace {
                pk: SignatureDelta::Sig(signature()),
                kek: SignatureDeltaVec::Default,
                db: SignatureDeltaVec::Sigs(Vec::new()),
                dbx: SignatureDeltaVec::Sigs(Vec::new()),
                moklist: None,
                moklistx: None,
            }),
            custom_vars: Vec::new(),
        };
        assert_eq!(
            partial_replace.secure_boot_customization(),
            SecureBootCustomization::PartialReplace
        );

        let full_replace = CustomVarsDelta {
            signatures: SignaturesDelta::Replace(SignaturesReplace {
                pk: SignatureDelta::Sig(signature()),
                kek: SignatureDeltaVec::Sigs(Vec::new()),
                db: SignatureDeltaVec::Sigs(Vec::new()),
                dbx: SignatureDeltaVec::Sigs(Vec::new()),
                moklist: None,
                moklistx: None,
            }),
            custom_vars: Vec::new(),
        };
        assert_eq!(
            full_replace.secure_boot_customization(),
            SecureBootCustomization::FullReplace
        );
    }
}
