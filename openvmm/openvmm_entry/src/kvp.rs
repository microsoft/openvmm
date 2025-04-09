// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to handle KVP (Key-Value Pair) operations.

use hyperv_ic_resources::kvp::KvpConnectRpc;
use mesh::CancelContext;
use mesh::rpc::RpcSend as _;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::time::Duration;

#[derive(clap::Args)]
pub(crate) struct KvpCommand {
    /// The timeout in seconds.
    #[clap(long, default_value = "3")]
    timeout: u64,
    #[clap(subcommand)]
    command: KvpSubcommand,
}

#[derive(clap::Subcommand)]
enum KvpSubcommand {
    /// Set a key/value pair in the KVP store.
    Set {
        /// The pool to use.
        #[clap(long)]
        pool: KvpPool,

        /// The key to set the value of.
        key: String,

        #[clap(long = "type", short = 't', default_value = "string")]
        value_type: ValueType,

        /// The value to set.
        value: String,
    },
    /// Get a key/value pair from the KVP store.
    Get {
        /// The pool to use.
        #[clap(long)]
        pool: KvpPool,

        /// The key to get the value of.
        key: String,
    },
    /// Enumerate the key/value pairs in the KVP store.
    Enum {
        /// The pool to use.
        #[clap(long)]
        pool: KvpPool,
    },
    /// Get IP address information for a given adapter.
    IpInfo {
        /// The MAC address to get the IP info for.
        adapter_id: String,
    },
}

#[derive(clap::ValueEnum, Clone)]
enum KvpPool {
    Guest,
    External,
    Auto,
    AutoExternal,
}

#[derive(clap::ValueEnum, Clone)]
enum ValueType {
    String,
    Dword,
    Qword,
}

pub(crate) async fn handle_kvp(
    kvp: &mesh::Sender<KvpConnectRpc>,
    command: KvpCommand,
) -> anyhow::Result<()> {
    let KvpCommand { timeout, command } = command;
    CancelContext::new()
        .with_timeout(Duration::from_secs(timeout))
        .until_cancelled(handle_subcommand(kvp, command))
        .await?
}

async fn handle_subcommand(
    kvp: &mesh::Sender<KvpConnectRpc>,
    command: KvpSubcommand,
) -> anyhow::Result<()> {
    let (kvp, _) = kvp.call_failable(KvpConnectRpc::WaitForGuest, ()).await?;
    match command {
        KvpSubcommand::Set {
            pool,
            key,
            value_type,
            value,
        } => {
            let pool = pool_cvt(pool);
            let value = match value_type {
                ValueType::String => hyperv_ic_resources::kvp::Value::String(value),
                ValueType::Dword => hyperv_ic_resources::kvp::Value::U32(value.parse()?),
                ValueType::Qword => hyperv_ic_resources::kvp::Value::U64(value.parse()?),
            };
            kvp.call_failable(
                hyperv_ic_resources::kvp::KvpRpc::Set,
                hyperv_ic_resources::kvp::SetParams { pool, key, value },
            )
            .await?;
        }
        KvpSubcommand::Get { pool, key } => {
            // Can you believe it? They never implemented the get
            // operation in the guest. Enumerate instead.
            let pool = pool_cvt(pool);
            for i in 0.. {
                match kvp
                    .call_failable(
                        hyperv_ic_resources::kvp::KvpRpc::Enumerate,
                        hyperv_ic_resources::kvp::EnumerateParams { pool, index: i },
                    )
                    .await?
                {
                    Some(v) if v.key == key => {
                        println!("{}", DisplayValue(&v.value));
                        break;
                    }
                    Some(_) => {
                        // Do nothing, continue searching
                    }
                    None => {
                        anyhow::bail!("not found");
                    }
                }
            }
        }
        KvpSubcommand::Enum { pool } => {
            let pool = pool_cvt(pool);
            for i in 0.. {
                match kvp
                    .call_failable(
                        hyperv_ic_resources::kvp::KvpRpc::Enumerate,
                        hyperv_ic_resources::kvp::EnumerateParams { pool, index: i },
                    )
                    .await?
                {
                    Some(v) => {
                        println!("{}: {}", v.key, DisplayValue(&v.value));
                    }
                    None => break,
                }
            }
        }
        KvpSubcommand::IpInfo { adapter_id } => {
            let ip_info = kvp
                .call_failable(
                    hyperv_ic_resources::kvp::KvpRpc::GetIpInfo,
                    hyperv_ic_resources::kvp::GetIpInfoParams { adapter_id },
                )
                .await?;
            let hyperv_ic_resources::kvp::IpInfo {
                ipv4,
                ipv6,
                dhcp_enabled,
                ipv4_addresses,
                ipv6_addresses,
                ipv4_gateways,
                ipv6_gateways,
                ipv4_dns_servers,
                ipv6_dns_servers,
            } = ip_info;
            if dhcp_enabled {
                println!("DHCP enabled");
            } else {
                println!("DHCP disabled");
            }
            let origin_str = |origin| match origin {
                hyperv_ic_resources::kvp::AddressOrigin::Unknown => "unknown",
                hyperv_ic_resources::kvp::AddressOrigin::Other => "other",
                hyperv_ic_resources::kvp::AddressOrigin::Static => "static",
            };
            if ipv4 {
                let a = Ipv4Addr::from;
                println!("IPv4:");
                for addr in ipv4_addresses {
                    println!(
                        "  {}/{} (origin {})",
                        a(addr.address),
                        a(addr.subnet),
                        origin_str(addr.origin)
                    );
                }
                for gw in ipv4_gateways {
                    println!("  Gateway: {}", a(gw));
                }
                for dns in ipv4_dns_servers {
                    println!("  DNS: {}", a(dns));
                }
            }
            if ipv6 {
                let a = Ipv6Addr::from;
                println!("IPv6:");
                for addr in ipv6_addresses {
                    println!(
                        "  {}/{} (origin {})",
                        a(addr.address),
                        addr.subnet,
                        origin_str(addr.origin)
                    );
                }
                for gw in ipv6_gateways {
                    println!("  Gateway: {}", a(gw));
                }
                for dns in ipv6_dns_servers {
                    println!("  DNS: {}", a(dns));
                }
            }
        }
    }
    Ok(())
}

fn pool_cvt(pool: KvpPool) -> hyperv_ic_resources::kvp::KvpPool {
    match pool {
        KvpPool::Guest => hyperv_ic_resources::kvp::KvpPool::Guest,
        KvpPool::External => hyperv_ic_resources::kvp::KvpPool::External,
        KvpPool::Auto => hyperv_ic_resources::kvp::KvpPool::Auto,
        KvpPool::AutoExternal => hyperv_ic_resources::kvp::KvpPool::AutoExternal,
    }
}

struct DisplayValue<'a>(&'a hyperv_ic_resources::kvp::Value);

impl std::fmt::Display for DisplayValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            hyperv_ic_resources::kvp::Value::String(s) => write!(f, "\"{s}\""),
            hyperv_ic_resources::kvp::Value::U32(v) => write!(f, "{v}"),
            hyperv_ic_resources::kvp::Value::U64(v) => write!(f, "{v}"),
        }
    }
}
