// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// See also the LICENSE file in the root of the crate for additional copyright
// information.

use super::params::OSSL_ALG_PARAM_CIPHER;
use super::params::OSSL_ALG_PARAM_DIGEST;
use super::params::OSSL_ALG_PARAM_MAC;
use super::params::OSSL_ALG_PARAM_PROPERTIES;
use std::ffi::CStr;

/* KDF / PRF parameters */
pub const OSSL_KDF_PARAM_SECRET: &CStr = c"secret";
pub const OSSL_KDF_PARAM_KEY: &CStr = c"key";
pub const OSSL_KDF_PARAM_SALT: &CStr = c"salt";
pub const OSSL_KDF_PARAM_PASSWORD: &CStr = c"pass";
pub const OSSL_KDF_PARAM_DIGEST: &CStr = OSSL_ALG_PARAM_DIGEST;
pub const OSSL_KDF_PARAM_CIPHER: &CStr = OSSL_ALG_PARAM_CIPHER;
pub const OSSL_KDF_PARAM_MAC: &CStr = OSSL_ALG_PARAM_MAC;
pub const OSSL_KDF_PARAM_MAC_SIZE: &CStr = c"maclen";
pub const OSSL_KDF_PARAM_PROPERTIES: &CStr = OSSL_ALG_PARAM_PROPERTIES;
pub const OSSL_KDF_PARAM_ITER: &CStr = c"iter";
pub const OSSL_KDF_PARAM_MODE: &CStr = c"mode";
pub const OSSL_KDF_PARAM_PKCS5: &CStr = c"pkcs5";
pub const OSSL_KDF_PARAM_UKM: &CStr = c"ukm";
pub const OSSL_KDF_PARAM_CEK_ALG: &CStr = c"cekalg";
pub const OSSL_KDF_PARAM_SCRYPT_N: &CStr = c"n";
pub const OSSL_KDF_PARAM_SCRYPT_R: &CStr = c"r";
pub const OSSL_KDF_PARAM_SCRYPT_P: &CStr = c"p";
pub const OSSL_KDF_PARAM_SCRYPT_MAXMEM: &CStr = c"maxmem_bytes";
pub const OSSL_KDF_PARAM_INFO: &CStr = c"info";
pub const OSSL_KDF_PARAM_SEED: &CStr = c"seed";
pub const OSSL_KDF_PARAM_SSHKDF_XCGHASH: &CStr = c"xcghash";
pub const OSSL_KDF_PARAM_SSHKDF_SESSION_ID: &CStr = c"session_id";
pub const OSSL_KDF_PARAM_SSHKDF_TYPE: &CStr = c"type";
pub const OSSL_KDF_PARAM_SIZE: &CStr = c"size";
pub const OSSL_KDF_PARAM_CONSTANT: &CStr = c"constant";
pub const OSSL_KDF_PARAM_PKCS12_ID: &CStr = c"id";
pub const OSSL_KDF_PARAM_KBKDF_USE_L: &CStr = c"use-l";
pub const OSSL_KDF_PARAM_KBKDF_USE_SEPARATOR: &CStr = c"use-separator";
pub const OSSL_KDF_PARAM_X942_PARTYUINFO: &CStr = c"partyu-info";
pub const OSSL_KDF_PARAM_X942_PARTYVINFO: &CStr = c"partyv-info";
pub const OSSL_KDF_PARAM_X942_SUPP_PUBINFO: &CStr = c"supp-pubinfo";
pub const OSSL_KDF_PARAM_X942_SUPP_PRIVINFO: &CStr = c"supp-privinfo";
pub const OSSL_KDF_PARAM_X942_USE_KEYBITS: &CStr = c"use-keybits";

/* Known KDF names */
pub const OSSL_KDF_NAME_HKDF: &CStr = c"HKDF";
pub const OSSL_KDF_NAME_PBKDF2: &CStr = c"PBKDF2";
pub const OSSL_KDF_NAME_SCRYPT: &CStr = c"SCRYPT";
pub const OSSL_KDF_NAME_SSHKDF: &CStr = c"SSHKDF";
pub const OSSL_KDF_NAME_SSKDF: &CStr = c"SSKDF";
pub const OSSL_KDF_NAME_TLS1_PRF: &CStr = c"TLS1-PRF";
pub const OSSL_KDF_NAME_X942KDF_ASN1: &CStr = c"X942KDF-ASN1";
pub const OSSL_KDF_NAME_X942KDF_CONCAT: &CStr = c"X942KDF-CONCAT";
pub const OSSL_KDF_NAME_X963KDF: &CStr = c"X963KDF";
pub const OSSL_KDF_NAME_KBKDF: &CStr = c"KBKDF";
pub const OSSL_KDF_NAME_KRB5KDF: &CStr = c"KRB5KDF";
