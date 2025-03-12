// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guid::Guid;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::ptr::null_mut;
use thiserror::Error;

pal::delayload!("computenetwork.dll" {
    fn HcnOpenNetwork(id: &Guid, network: &mut *mut c_void, error_record: *mut *mut u16) -> i32;
    fn HcnCloseNetwork(network: NonNull<c_void>) -> i32;
});

#[derive(Debug, Error)]
#[error("HCN {0} failed", operation)]
pub struct Error {
    operation: &'static str,
    #[source]
    err: std::io::Error,
}

fn chk(operation: &'static str, result: i32) -> Result<i32, Error> {
    if result >= 0 {
        Ok(result)
    } else {
        Err(Error {
            operation,
            err: std::io::Error::from_raw_os_error(result),
        })
    }
}

pub struct Network(NonNull<c_void>);

impl Network {
    pub fn open(id: &Guid) -> Result<Self, Error> {
        let mut network = null_mut();
        chk("open", unsafe {
            HcnOpenNetwork(id, &mut network, null_mut())
        })?;
        Ok(Self(
            NonNull::new(network).expect("HcnOpenNetwork returned null network"),
        ))
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        if let Err(e) = chk("close", unsafe { HcnCloseNetwork(self.0) }) {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "failed to close HCN network"
            );
        }
    }
}
