// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides helper functions for bridging between vsock/hvsocket and Unix domain sockets, utilized
//! by VMBus-based hvsocket and virtio-vsock.

use fs_err::PathExt;
use guid::Guid;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

/// The maximum length of a valid connect request. It could be shorter if it contains a port number
/// instead of a service ID.
pub const HYBRID_CONNECT_REQUEST_LEN: usize =
    "CONNECT 00000000-facb-11e6-bd58-64006a7986d3\n".len();

// This GUID is an embedding of the AF_VSOCK port into an AF_HYPERV service ID.
const VSOCK_TEMPLATE: Guid = guid::guid!("00000000-facb-11e6-bd58-64006a7986d3");

/// Defines a connection request for a vsock or hvsocket connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionRequest {
    /// A connection request for a vsock port number.
    Port(u32),
    /// A connection request for an hvsocket service ID.
    ServiceId(Guid),
}

impl ConnectionRequest {
    /// Gets the vsock port number associated with this connection request. This will return a value
    /// if the request either directly uses a port, or uses a service ID that matches the hvsocket
    /// vsock template.
    pub fn port(&self) -> Option<u32> {
        match self {
            ConnectionRequest::Port(port) => Some(*port),
            ConnectionRequest::ServiceId(service_id) => {
                let stripped_id = Guid {
                    data1: 0,
                    ..*service_id
                };
                (VSOCK_TEMPLATE == stripped_id).then_some(service_id.data1)
            }
        }
    }

    /// Gets the vsock service ID associated with this connection request. If this connection
    /// request is for a port, it will use the hvsocket vsock template to construct a service ID.
    pub fn service_id(&self) -> Guid {
        match self {
            ConnectionRequest::Port(port) => Guid {
                data1: *port,
                ..VSOCK_TEMPLATE
            },
            ConnectionRequest::ServiceId(service_id) => *service_id,
        }
    }

    /// Gets the path of a Unix domain socket on the host that is listening for this connection
    /// request.
    ///
    /// If the path uses a vsock port number, either directly or through the hvsocket vsock
    /// template, then a listener using the port number will be given preference over one using the
    /// service ID.
    pub fn host_uds_path(&self, base_path: impl AsRef<Path>) -> Result<PathBuf, UdsPathError> {
        let base_path = base_path.as_ref();
        let mut path = base_path.as_os_str().to_owned();
        if let Some(port) = self.port() {
            // This is a vsock connection, so first try connecting after appending the
            // port to the path.
            path.push(format!("_{port}"));
            if Path::new(&path).fs_err_try_exists()? {
                return Ok(path.into());
            }

            // If the port didn't exist, try again with the service ID.
            path.clear();
            path.push(base_path);
        }

        path.push(format!("_{}", self.service_id()));
        if !Path::new(&path).fs_err_try_exists()? {
            return Err(UdsPathError::NoListener(base_path.to_path_buf()));
        }

        Ok(path.into())
    }

    /// Parses a connection request from a buffer containing a UTF-8 string of the format "CONNECT <port or service ID>\n".
    pub fn parse_connect_request(buf: &[u8]) -> Result<Self, ParseError> {
        let rest = buf
            .strip_prefix(b"CONNECT ")
            .ok_or(ParseError::MissingPrefix)?;

        let rest = std::str::from_utf8(rest).map_err(ParseError::InvalidString)?;
        if let Ok(port) = u32::from_str(rest) {
            Ok(ConnectionRequest::Port(port))
        } else if let Ok(service_id) = Guid::from_str(rest) {
            Ok(ConnectionRequest::ServiceId(service_id))
        } else {
            Err(ParseError::InvalidFormat(rest.to_string()))
        }
    }
}

/// Error returned by [`ConnectionRequest::host_uds_path`].
#[derive(Debug, thiserror::Error)]
pub enum UdsPathError {
    /// No hybrid vsock listener was found at the base path.
    #[error("no hybrid vsock listener based at {}", _0.display())]
    NoListener(PathBuf),
    /// An I/O error occurred while checking for the listener.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Error returned by [`ConnectionRequest::parse_connect_request`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The connect request did not contain a newline within the maximum expected length.
    #[error("connect request did not fit")]
    RequestTooLong,
    /// The connect request did not start with the expected "CONNECT " prefix.
    #[error("missing CONNECT prefix")]
    MissingPrefix,
    /// The connect request contained invalid UTF-8.
    #[error("invalid UTF-8 in connect request")]
    InvalidString(#[from] std::str::Utf8Error),
    /// The connect request did not contain a valid port number or service ID.
    #[error("invalid port or service ID: {0}")]
    InvalidFormat(String),
}
