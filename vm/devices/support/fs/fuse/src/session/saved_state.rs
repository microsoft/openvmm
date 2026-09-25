// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serializable FUSE session state: the negotiated protocol values, their
//! validation, and saving and restoring them on a session.

use super::Session;
use super::SessionInfo;
use crate::protocol::FUSE_INIT_EXT;
use crate::protocol::FUSE_KERNEL_MINOR_VERSION;
use crate::protocol::FUSE_KERNEL_VERSION;
use std::sync::atomic;
use thiserror::Error;

/// Serializable FUSE session information.
///
/// This is intentionally independent from any transport or filesystem
/// implementation. It contains only negotiated protocol values, never a file
/// handle, task, or native resource.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionInfoState {
    /// Negotiated FUSE major version.
    pub major: u32,
    /// Negotiated FUSE minor version.
    pub minor: u32,
    /// Guest maximum readahead.
    pub max_readahead: u32,
    /// Feature flags advertised by the guest.
    pub capable: u32,
    /// Extended feature flags advertised by the guest.
    pub capable2: u32,
    /// Feature flags accepted by the server.
    pub want: u32,
    /// Extended feature flags accepted by the server.
    pub want2: u32,
    /// Maximum background requests.
    pub max_background: u16,
    /// Congestion threshold.
    pub congestion_threshold: u16,
    /// Maximum write size.
    pub max_write: u32,
    /// Timestamp granularity.
    pub time_gran: u32,
}

/// Serializable state of a FUSE session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionState {
    /// Whether the session has completed FUSE_INIT.
    pub initialized: bool,
    /// Negotiated session information.
    pub info: SessionInfoState,
}

/// Invalid FUSE session state.
#[derive(Debug, Error)]
pub enum SessionStateError {
    /// The state does not describe a supported FUSE protocol.
    #[error("unsupported FUSE protocol version")]
    UnsupportedVersion,
    /// Accepted feature bits were not advertised by the guest.
    #[error("FUSE negotiated flags are not a subset of guest capabilities")]
    InvalidFlags,
    /// The extended-init flags are inconsistent.
    #[error("FUSE extended-init flags are inconsistent")]
    InvalidExtendedFlags,
    /// A negotiated protocol limit is invalid.
    #[error("FUSE negotiated protocol limit is invalid")]
    InvalidLimits,
    /// An uninitialized session retained negotiation state.
    #[error("uninitialized FUSE session retained negotiation state")]
    UninitializedWithInfo,
}

impl SessionState {
    /// Validates that the state can be restored by this implementation.
    pub fn validate(&self) -> Result<(), SessionStateError> {
        let info = self.info;
        if !self.initialized {
            return if info == SessionInfoState::default() {
                Ok(())
            } else {
                Err(SessionStateError::UninitializedWithInfo)
            };
        }
        if info.major != FUSE_KERNEL_VERSION
            || !(27..=FUSE_KERNEL_MINOR_VERSION).contains(&info.minor)
        {
            return Err(SessionStateError::UnsupportedVersion);
        }
        if info.want & !info.capable != 0 {
            return Err(SessionStateError::InvalidFlags);
        }
        if info.capable & FUSE_INIT_EXT == 0 {
            if info.capable2 != 0 || info.want2 != 0 {
                return Err(SessionStateError::InvalidExtendedFlags);
            }
        } else if info.want & FUSE_INIT_EXT == 0 {
            if info.want2 != 0 {
                return Err(SessionStateError::InvalidExtendedFlags);
            }
        } else if info.want2 & !info.capable2 != 0 {
            return Err(SessionStateError::InvalidExtendedFlags);
        }
        if info.max_write == 0 || info.time_gran == 0 {
            return Err(SessionStateError::InvalidLimits);
        }
        Ok(())
    }
}

impl Session {
    /// Saves typed negotiated protocol state for device-private migration.
    pub fn save_state(&self) -> SessionState {
        let initialized = self.is_initialized();
        let info = SessionInfoState::from(*self.info.read());
        SessionState { initialized, info }
    }

    /// Restores a previously validated FUSE negotiation.
    ///
    /// The caller must ensure no request dispatch is active while invoking
    /// this method.
    pub fn restore_state(&self, state: SessionState) -> Result<(), SessionStateError> {
        state.validate()?;
        *self.info.write() = state.info.into();
        self.initialized
            .store(state.initialized, atomic::Ordering::Release);
        Ok(())
    }
}

impl From<SessionInfo> for SessionInfoState {
    fn from(info: SessionInfo) -> Self {
        Self {
            major: info.major,
            minor: info.minor,
            max_readahead: info.max_readahead,
            capable: info.capable,
            capable2: info.capable2,
            want: info.want,
            want2: info.want2,
            max_background: info.max_background,
            congestion_threshold: info.congestion_threshold,
            max_write: info.max_write,
            time_gran: info.time_gran,
        }
    }
}

impl From<SessionInfoState> for SessionInfo {
    fn from(state: SessionInfoState) -> Self {
        Self {
            major: state.major,
            minor: state.minor,
            max_readahead: state.max_readahead,
            capable: state.capable,
            capable2: state.capable2,
            want: state.want,
            want2: state.want2,
            max_background: state.max_background,
            congestion_threshold: state.congestion_threshold,
            max_write: state.max_write,
            time_gran: state.time_gran,
        }
    }
}
