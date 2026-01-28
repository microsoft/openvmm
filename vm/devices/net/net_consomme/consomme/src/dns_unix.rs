// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use resolv_conf::ScopedIp;
use smoltcp::wire::Ipv4Address;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failing reading resolv.conf")]
    Io(#[from] std::io::Error),
    #[error("failing parsing resolv.conf")]
    Parse(#[from] resolv_conf::ParseError),
}

pub fn nameservers() -> Result<Vec<Ipv4Address>, Error> {
    let contents = std::fs::read("/etc/resolv.conf")?;
    let config = resolv_conf::Config::parse(contents)?;
    Ok(config
        .nameservers
        .iter()
        .filter_map(|ns| match ns {
            ScopedIp::V4(addr) => Some(*addr),
            ScopedIp::V6(_, _) => None,
        })
        .collect())
}
