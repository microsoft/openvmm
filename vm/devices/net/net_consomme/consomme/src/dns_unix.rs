// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use resolv_conf::ScopedIp;
use smoltcp::wire::IpAddress;
use smoltcp::wire::Ipv4Address;
use smoltcp::wire::Ipv6Address;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failing reading resolv.conf")]
    Io(#[from] std::io::Error),
    #[error("failing parsing resolv.conf")]
    Parse(#[from] resolv_conf::ParseError),
}

pub fn nameservers() -> Result<Vec<IpAddress>, Error> {
    let contents = std::fs::read("/etc/resolv.conf")?;
    let config = resolv_conf::Config::parse(contents)?;
    Ok(config
        .nameservers
        .iter()
        .filter_map(|ns| match ns {
            ScopedIp::V4(addr) => Some(IpAddress::Ipv4(Ipv4Address::from(*addr))),
            ScopedIp::V6(addr, None) => Some(IpAddress::Ipv6(Ipv6Address::from(*addr))),
            ScopedIp::V6(addr, Some(scope)) => {
                tracelimit::warn_ratelimited!(
                    %addr,
                    scope,
                    "ignoring scoped IPv6 nameserver"
                );
                None
            }
        })
        .collect())
}
