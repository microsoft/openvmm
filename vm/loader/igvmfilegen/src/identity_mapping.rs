// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements creation of JSON formatted launch measurement identity documents.
//! This comes from Intel's TD Identity Mapping, and has been expanded
//! for SNP and VBS.
//! This format allows verifiers to validate a COSE Sign1 payload,
//! and correlate that to a launch measurement and SVN.
use serde::Serialize;

/// Defined by specification.
const CLASS_ID_GUID: &str = "7fb00ee4-a7ff-11ed-9e2f-00155d09de56";

/// This field is required by TD mapping specification.
#[derive(Serialize, Debug)]
pub struct Environment {
    pub class_id: String,
}

/// SNP launch measurement.
#[derive(Serialize, Debug, Clone)]
pub struct SnpLaunchMeasurement {
    #[serde(
        serialize_with = "hex::serde::serialize_upper",
        deserialize_with = "hex::serde::deserialize"
    )]
    pub snp_ld: [u8; 48],
}

/// TDX MRTD.
#[derive(Serialize, Debug, Clone)]
pub struct TdxLaunchMeasurement {
    #[serde(
        serialize_with = "hex::serde::serialize_upper",
        deserialize_with = "hex::serde::deserialize"
    )]
    pub tdx_mrtd: [u8; 48],
}

/// VBS Boot Digest.
#[derive(Serialize, Debug, Clone)]
pub struct VbsLaunchMeasurement {
    #[serde(
        serialize_with = "hex::serde::serialize_upper",
        deserialize_with = "hex::serde::deserialize"
    )]
    pub vbs_boot_digest: [u8; 32],
}

/// Build information.
#[derive(Serialize, Debug, Clone)]
pub struct BuildInfo {
    pub debug_build: bool,
}

/// SVN of this image.
#[derive(Serialize, Debug, Clone)]
pub struct SnpEndorsement {
    pub snp_isvsvn: u32,
    pub build_info: BuildInfo,
}

/// SVN of this image.
#[derive(Serialize, Debug, Clone)]
pub struct TdxEndorsement {
    pub tdx_isvsvn: u32,
    pub build_info: BuildInfo,
}

/// SVN of this image.
#[derive(Serialize, Debug, Clone)]
pub struct VbsEndorsement {
    pub vbs_isvsvn: u32,
    pub build_info: BuildInfo,
}

#[derive(Serialize, Debug, Clone)]
pub struct MeasurementInstance<R, E> {
    pub reference: R,
    pub endorsement: E,
}

#[derive(Serialize, Debug)]
pub struct BaseMeasurement<R, E> {
    pub environment: Environment,
    pub series: Vec<MeasurementInstance<R, E>>,
}

/// Combined measurement structure.
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum Measurement {
    Snp(BaseMeasurement<SnpLaunchMeasurement, SnpEndorsement>),
    Tdx(BaseMeasurement<TdxLaunchMeasurement, TdxEndorsement>),
    Vbs(BaseMeasurement<VbsLaunchMeasurement, VbsEndorsement>),
}

pub type SnpMeasurement = BaseMeasurement<SnpLaunchMeasurement, SnpEndorsement>;
pub type TdxMeasurement = BaseMeasurement<TdxLaunchMeasurement, TdxEndorsement>;
pub type VbsMeasurement = BaseMeasurement<VbsLaunchMeasurement, VbsEndorsement>;

impl SnpMeasurement {
    /// SNP measurement and endorsements.
    pub fn new(ld: [u8; 48], svn: u32, debug_enabled: bool) -> Self {
        let info = BuildInfo {
            debug_build: debug_enabled,
        };
        let measurements: MeasurementInstance<_, _> = MeasurementInstance {
            reference: SnpLaunchMeasurement { snp_ld: ld },
            endorsement: SnpEndorsement {
                snp_isvsvn: svn,
                build_info: info,
            },
        };
        BaseMeasurement {
            environment: Environment {
                class_id: CLASS_ID_GUID.to_string(),
            },
            series: [measurements].to_vec(),
        }
    }
}
impl TdxMeasurement {
    /// TDX measurement and endorsements.
    pub fn new(mrtd: [u8; 48], svn: u32, debug_enabled: bool) -> Self {
        let info = BuildInfo {
            debug_build: debug_enabled,
        };
        let measurements: MeasurementInstance<_, _> = MeasurementInstance {
            reference: TdxLaunchMeasurement { tdx_mrtd: mrtd },
            endorsement: TdxEndorsement {
                tdx_isvsvn: svn,
                build_info: info,
            },
        };
        BaseMeasurement {
            environment: Environment {
                class_id: CLASS_ID_GUID.to_string(),
            },
            series: [measurements].to_vec(),
        }
    }
}

impl VbsMeasurement {
    /// VBS measurement and endorsements.
    pub fn new(digest: [u8; 32], svn: u32, debug_enabled: bool) -> Self {
        let info = BuildInfo {
            debug_build: debug_enabled,
        };
        let measurements: MeasurementInstance<_, _> = MeasurementInstance {
            reference: VbsLaunchMeasurement {
                vbs_boot_digest: digest,
            },
            endorsement: VbsEndorsement {
                vbs_isvsvn: svn,
                build_info: info,
            },
        };
        BaseMeasurement {
            environment: Environment {
                class_id: CLASS_ID_GUID.to_string(),
            },
            series: [measurements].to_vec(),
        }
    }
}
