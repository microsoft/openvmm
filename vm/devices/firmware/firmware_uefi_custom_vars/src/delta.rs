// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data types which define a "delta" operation on a
//! [`CustomVars`](super::CustomVars) struct.

use super::CustomVar;
use super::Signature;

/// Collection of custom UEFI nvram variables.
#[derive(Debug)]
pub struct CustomVarsDelta {
    /// Secure Boot signature vars
    pub signatures: SignaturesDelta,
    /// Any additional custom vars
    pub custom_vars: Vec<(String, CustomVar)>,
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
