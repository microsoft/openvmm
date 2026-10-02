// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPU profile failures, with the stable codes of the time ABI specification.

use std::fmt;

/// A stable failure code from the "Failure codes" table of the NVX time ABI
/// specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileErrorCode {
    /// `E_PROFILE_UNKNOWN`: the profile ID is not pinned in this OpenVMM.
    ProfileUnknown,
    /// `E_PROFILE_DIGEST`: a profile or effective-CPUID record does not
    /// verify against its digest, does not decode, or disagrees with the
    /// pinned profile of the same ID.
    ProfileDigest,
    /// `E_PROFILE_HOST_UNKNOWN`: automatic selection maps the host CPU to no
    /// pinned profile.
    ProfileHostUnknown,
    /// `E_CPU_GENERATION`: the host CPU is not in the profile's generation.
    CpuGeneration,
    /// `E_PROFILE_UNSUPPORTED`: the backend lacks a feature, limit, XSAVE
    /// layout, or MSR value of the profile.
    ProfileUnsupported,
    /// `E_CPU_SURFACE`: an effective CPUID differs from the recorded one, or
    /// the VM's CPUID inputs cannot complete the profile.
    CpuSurface,
    /// `E_CPU_UNLISTED`: a pass-through backend presents a non-zero CPUID
    /// entry outside the profile's tables.
    CpuUnlisted,
}

impl ProfileErrorCode {
    /// Every code, in specification order.
    pub const ALL: [Self; 7] = [
        Self::ProfileUnknown,
        Self::ProfileDigest,
        Self::ProfileHostUnknown,
        Self::CpuGeneration,
        Self::ProfileUnsupported,
        Self::CpuSurface,
        Self::CpuUnlisted,
    ];

    /// Returns the code as the specification spells it, such as
    /// `E_PROFILE_UNKNOWN`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProfileUnknown => "E_PROFILE_UNKNOWN",
            Self::ProfileDigest => "E_PROFILE_DIGEST",
            Self::ProfileHostUnknown => "E_PROFILE_HOST_UNKNOWN",
            Self::CpuGeneration => "E_CPU_GENERATION",
            Self::ProfileUnsupported => "E_PROFILE_UNSUPPORTED",
            Self::CpuSurface => "E_CPU_SURFACE",
            Self::CpuUnlisted => "E_CPU_UNLISTED",
        }
    }
}

impl fmt::Display for ProfileErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A CPU profile failure.
///
/// It displays as `[E_CODE] message`, the format of the time ABI's errors, so
/// the code survives when the error crosses a process boundary as a string.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct ProfileError {
    /// The stable code.
    pub code: ProfileErrorCode,
    /// What failed.
    pub message: String,
}

impl ProfileError {
    /// Returns an error with `code` and `message`.
    pub fn new(code: ProfileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileError;
    use super::ProfileErrorCode;
    use test_with_tracing::test;

    #[test]
    fn displays_the_bracketed_code_first() {
        let error = ProfileError::new(ProfileErrorCode::CpuGeneration, "host is 6/85/7");
        assert_eq!(error.to_string(), "[E_CPU_GENERATION] host is 6/85/7");
    }

    #[test]
    fn codes_are_distinct_and_spelled_like_the_specification() {
        let mut codes = ProfileErrorCode::ALL.map(ProfileErrorCode::as_str).to_vec();
        assert!(codes.iter().all(|code| {
            code.starts_with("E_")
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        }));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), ProfileErrorCode::ALL.len());
    }
}
