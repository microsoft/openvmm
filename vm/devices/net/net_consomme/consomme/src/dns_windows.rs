// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling Win32 APIs to get DNS server information.
#![expect(unsafe_code)]

use smoltcp::wire::IpAddress;
use std::alloc::Layout;
use std::io;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::ptr::NonNull;
use std::ptr::null_mut;
use thiserror::Error;
use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::GAA_FLAG_INCLUDE_ALL_INTERFACES;
use windows_sys::Win32::NetworkManagement::IpHelper::GAA_FLAG_SKIP_ANYCAST;
use windows_sys::Win32::NetworkManagement::IpHelper::GAA_FLAG_SKIP_FRIENDLY_NAME;
use windows_sys::Win32::NetworkManagement::IpHelper::GAA_FLAG_SKIP_MULTICAST;
use windows_sys::Win32::NetworkManagement::IpHelper::GAA_FLAG_SKIP_UNICAST;
use windows_sys::Win32::NetworkManagement::IpHelper::GetAdaptersAddresses;
use windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_ADDRESSES_LH;
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::Networking::WinSock::AF_INET6;
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
use windows_sys::Win32::Networking::WinSock::SOCKADDR_IN;
use windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to query adapter DNS addresses")]
    AdapterAddresses(#[source] io::Error),
}

pub fn nameservers() -> Result<Vec<IpAddress>, Error> {
    let flags = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_FRIENDLY_NAME
        | GAA_FLAG_INCLUDE_ALL_INTERFACES;

    let mut dns_servers = Vec::new();

    // SAFETY: carefully calling APIs with properly aligned and sized buffers,
    // then walking the results, trusting that the Win32 API produces valid
    // buffers.
    unsafe {
        let mut addrs = Addresses::new(0);
        loop {
            let mut size = addrs.size();
            let r = GetAdaptersAddresses(
                AF_UNSPEC.into(),
                flags,
                null_mut(),
                addrs.as_ptr(),
                &mut size,
            );
            match r {
                ERROR_SUCCESS => break,
                ERROR_BUFFER_OVERFLOW => {}
                err => {
                    return Err(Error::AdapterAddresses(io::Error::from_raw_os_error(
                        err as i32,
                    )));
                }
            }

            assert!(size > addrs.size());
            drop(addrs);
            addrs = Addresses::new(size);
        }

        // Walk the data structure, finding all DNS servers.
        let mut addr_p = addrs.as_ptr();
        while !addr_p.is_null() {
            let addr = &*addr_p;
            let mut dns_p = addr.FirstDnsServerAddress;
            while !dns_p.is_null() {
                let dns = &*dns_p;
                let dns_addr = &*dns.Address.lpSockaddr;
                if dns_addr.sa_family == AF_INET {
                    let dns_addr = &*dns.Address.lpSockaddr.cast::<SOCKADDR_IN>();
                    let ipv4_addr = Ipv4Addr::from(u32::from_be(dns_addr.sin_addr.S_un.S_addr));
                    dns_servers.push(ipv4_addr.into());
                } else if dns_addr.sa_family == AF_INET6 {
                    let dns_addr = &*dns.Address.lpSockaddr.cast::<SOCKADDR_IN6>();
                    let ipv6_addr = Ipv6Addr::from(u128::from_be_bytes(dns_addr.sin6_addr.u.Byte));
                    dns_servers.push(ipv6_addr.into());
                }
                dns_p = dns.Next;
            }
            addr_p = addr.Next;
        }
    }

    Ok(dns_servers)
}

struct Addresses {
    ptr: Option<NonNull<IP_ADAPTER_ADDRESSES_LH>>,
    layout: Layout,
}

impl Addresses {
    fn new(size: u32) -> Self {
        let layout =
            Layout::from_size_align(size as usize, align_of::<IP_ADAPTER_ADDRESSES_LH>()).unwrap();

        let ptr = if size != 0 {
            // SAFETY: size is known to be non-zero.
            Some(unsafe { NonNull::new(std::alloc::alloc(layout).cast()).unwrap() })
        } else {
            None
        };
        Self { ptr, layout }
    }

    fn as_ptr(&self) -> *mut IP_ADAPTER_ADDRESSES_LH {
        self.ptr.map_or(null_mut(), |p| p.as_ptr())
    }

    fn size(&self) -> u32 {
        self.layout.size() as u32
    }
}

impl Drop for Addresses {
    fn drop(&mut self) {
        if let Some(ptr) = self.ptr {
            // SAFETY: the pointer is owned.
            unsafe { std::alloc::dealloc(ptr.as_ptr().cast(), self.layout) }
        }
    }
}
