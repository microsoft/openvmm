// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::service::Code;
use crate::service::Status;
use thiserror::Error;

pub fn status_from_err(code: Code, err: impl Into<anyhow::Error>) -> Status {
    Status {
        code: code as i32,
        message: format!("{:#}", err.into()),
        details: Vec::new(),
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid message type {0}")]
    InvalidMessageType(u8),
    #[error("stream id must be odd for client requests")]
    EvenStreamId,
}
