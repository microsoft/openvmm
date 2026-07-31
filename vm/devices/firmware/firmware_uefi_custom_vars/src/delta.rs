// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data types which define a "delta" operation on a
//! [`UefiVars`](super::UefiVars) struct.

use super::Signature;
use super::UefiVar;

/// Changes to apply to a collection of UEFI nvram variables.
#[derive(Debug, Clone)]
pub struct UefiVarsDelta {
    /// Secure Boot signature vars
    pub signatures: SignaturesDelta,
    /// UEFI vars that are not Secure Boot signature vars
    pub non_signature_vars: Vec<(String, UefiVar)>,
}

#[derive(Debug, Clone)]
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

/// Replace the underlying template signatures, optionally using `Default` values
/// from a base template. If no base template is provided, all required signature
/// values must be specified explicitly.
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
