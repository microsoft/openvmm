// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A structure corresponding to a TAP interface.

// UNSAFETY: Interacting with a union in bindgen-generated code and calling an ioctl.
#![expect(unsafe_code)]

use crate::VirtioNetHdr;
use futures::AsyncRead;
use linux_net_bindings::gen_if;
use linux_net_bindings::gen_if_tun;
use linux_net_bindings::tun_get_iff;
use linux_net_bindings::tun_get_vnet_hdr_sz;
use linux_net_bindings::tun_set_iff;
use linux_net_bindings::tun_set_offload;
use linux_net_bindings::tun_set_vnet_hdr_sz;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::raw::c_short;
use std::os::unix::prelude::AsRawFd;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("TAP interface name is too long: {0:#}")]
    TapNameTooLong(usize),
    #[error("failed to open /dev/net/tun")]
    OpenTunFailed(#[source] io::Error),
    #[error("TUNSETIFF ioctl failed")]
    SetTapAttributes(#[source] io::Error),
    #[error("TUNGETIFF ioctl failed")]
    GetTapAttributes(#[source] io::Error),
    #[error("TUNGETVNETHDRSZ ioctl failed")]
    GetVnetHdrSize(#[source] io::Error),
    #[error("TUNSETVNETHDRSZ ioctl failed")]
    SetVnetHdrSize(#[source] io::Error),
    #[error("TUNSETOFFLOAD ioctl failed")]
    SetOffload(#[source] io::Error),
    #[error("TAP name conversion to C string failed")]
    TapNameConversion(#[source] std::ffi::NulError),
    #[error("TAP interface does not have IFF_VNET_HDR set")]
    NoVnetHdr,
    #[error("TAP interface has unexpected vnet header size {actual}, expected {expected}")]
    WrongVnetHdrSize { expected: usize, actual: usize },
}

/// Opens a TAP interface by name and returns the fd.
///
/// The fd is configured with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`.
pub fn open_tap(name: &str) -> Result<OwnedFd, Error> {
    let tap_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .map_err(Error::OpenTunFailed)?;

    let mut ifreq: gen_if::ifreq = Default::default();

    let tap_name_cstr = CString::new(name.as_bytes()).map_err(Error::TapNameConversion)?;
    let tap_name_bytes = tap_name_cstr.into_bytes_with_nul();
    let tap_name_length = tap_name_bytes.len();

    // SAFETY: the ifr_ifrn union has a single member, and using
    // ifr_ifrn is consistent with issuing the TUNSETIFF ioctl below.
    let name_slice = unsafe { ifreq.ifr_ifrn.ifrn_name.as_mut() };

    if name_slice.len() < tap_name_length {
        return Err(Error::TapNameTooLong(tap_name_length));
    }

    for i in 0..tap_name_length {
        name_slice[i] = tap_name_bytes[i] as libc::c_char;
    }
    ifreq.ifr_ifru.ifru_flags =
        (gen_if_tun::IFF_TAP | gen_if_tun::IFF_NO_PI | gen_if_tun::IFF_VNET_HDR) as c_short;

    // SAFETY: calling the ioctl according to implementation requirements.
    unsafe {
        tun_set_iff(tap_file.as_raw_fd(), &ifreq)
            .map_err(|_e| Error::SetTapAttributes(io::Error::last_os_error()))?;
    };

    let fd = OwnedFd::from(tap_file);
    Ok(fd)
}

/// Structure corresponding to a TAP interface.
///
/// Wraps a validated TAP fd with `IFF_VNET_HDR` and the correct vnet header
/// size. Offloads are configured by
/// [`TapEndpoint::new`](super::TapEndpoint::new) via [`Tap::set_offloads`].
#[derive(Debug)]
pub struct Tap {
    tap: File,
}

impl Tap {
    /// Wraps an already-open TAP fd and validates it.
    ///
    /// The fd must already have `TUNSETIFF` applied with `IFF_VNET_HDR`.
    /// This function will:
    /// - Query the fd with `TUNGETIFF` to verify `IFF_VNET_HDR` is set
    /// - Set the vnet header size to the 12-byte v1 format
    pub fn new(fd: OwnedFd) -> Result<Self, Error> {
        let tap: File = fd.into();

        // Verify IFF_VNET_HDR is set.
        let mut ifreq: gen_if::ifreq = Default::default();
        // SAFETY: calling the ioctl with a valid fd and zeroed ifreq.
        unsafe {
            tun_get_iff(tap.as_raw_fd(), &mut ifreq)
                .map_err(|_e| Error::GetTapAttributes(io::Error::last_os_error()))?;
        };
        // SAFETY: the ifr_ifru union was populated by the TUNGETIFF ioctl,
        // which writes the interface flags into ifru_flags.
        if unsafe { ifreq.ifr_ifru.ifru_flags } as u32 & gen_if_tun::IFF_VNET_HDR == 0 {
            return Err(Error::NoVnetHdr);
        }

        // Set the vnet header size to the 12-byte v1 format.
        let expected_sz = size_of::<VirtioNetHdr>() as std::os::raw::c_int;
        // SAFETY: calling the ioctl with a valid fd and correct argument type.
        unsafe {
            tun_set_vnet_hdr_sz(tap.as_raw_fd(), &expected_sz)
                .map_err(|_e| Error::SetVnetHdrSize(io::Error::last_os_error()))?;
        };

        // Verify the header size was applied.
        let mut actual_sz: std::os::raw::c_int = 0;
        // SAFETY: calling the ioctl with a valid fd and correct argument type.
        unsafe {
            tun_get_vnet_hdr_sz(tap.as_raw_fd(), &mut actual_sz)
                .map_err(|_e| Error::GetVnetHdrSize(io::Error::last_os_error()))?;
        };
        if actual_sz != expected_sz {
            return Err(Error::WrongVnetHdrSize {
                expected: expected_sz as usize,
                actual: actual_sz as usize,
            });
        }

        Ok(Self { tap })
    }

    /// Sets TX offload flags via `TUNSETOFFLOAD`.
    ///
    /// `flags` is a bitmask of `TUN_F_*` constants (e.g., `TUN_F_CSUM | TUN_F_TSO4`).
    pub fn set_offloads(&self, flags: u32) -> Result<(), Error> {
        // SAFETY: calling the ioctl with a valid fd and correct argument type.
        unsafe {
            tun_set_offload(self.tap.as_raw_fd(), flags as std::os::raw::c_int)
                .map_err(|_e| Error::SetOffload(io::Error::last_os_error()))?;
        };
        Ok(())
    }

    pub fn polled(self, driver: &(impl Driver + ?Sized)) -> io::Result<PolledTap> {
        Ok(PolledTap {
            tap: PolledPipe::new(driver, self.tap)?,
        })
    }
}

/// A version of [`Tap`] that implements [`AsyncRead`].
pub struct PolledTap {
    tap: PolledPipe,
}

impl PolledTap {
    pub fn into_inner(self) -> Tap {
        Tap {
            tap: self.tap.into_inner(),
        }
    }
}

impl AsyncRead for PolledTap {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.tap).poll_read(cx, buf)
    }
}

impl Write for PolledTap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // N.B. This will be a non-blocking write because `PolledPipe::new` puts
        // the file into nonblocking mode.
        self.tap.get().write(buf)
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.tap.get().write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
