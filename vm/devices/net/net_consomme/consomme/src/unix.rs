// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]
//! Unix implementation of host IPv6 address detection.
//!
//! Uses `getifaddrs()` from libc to check if the host has any non-link-local
//! IPv6 unicast addresses assigned.

// UNSAFETY: Calling libc getifaddrs/freeifaddrs and walking the resulting
// linked list of interface addresses.
#![expect(unsafe_code)]

use std::net::Ipv6Addr;

/// Checks whether the host has at least one non-link-local, non-loopback
/// IPv6 unicast address assigned.
pub fn host_has_ipv6_address() -> Result<bool, std::io::Error> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();

    // SAFETY: Calling getifaddrs according to its API contract. The function
    // allocates memory and populates a linked list of interface addresses.
    let result = unsafe { libc::getifaddrs(&mut addrs) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut found = false;
    let mut current = addrs;

    while !current.is_null() {
        // SAFETY: `current` is a valid node in the linked list allocated by
        // getifaddrs. We dereference it to read ifa_addr and ifa_next.
        // When ifa_addr is a non-null AF_INET6 sockaddr, we cast to
        // sockaddr_in6 to extract the address bytes.
        let (ipv6_addr, next) = unsafe {
            let ifa = &*current;
            let addr =
                if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET6 {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    Some(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                } else {
                    None
                };
            (addr, ifa.ifa_next)
        };

        if let Some(addr) = ipv6_addr {
            if super::is_routable_ipv6(&addr) {
                found = true;
                break;
            }
        }

        current = next;
    }

    // SAFETY: Freeing the linked list allocated by getifaddrs.
    unsafe { libc::freeifaddrs(addrs) };

    Ok(found)
}
