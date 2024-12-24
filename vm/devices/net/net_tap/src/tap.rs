// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A structure corresponding to a TAP interface.

// UNSAFETY: Interacting with a union in bindgen-generated code and calling an ioctl.
#![expect(unsafe_code)]

use futures::AsyncRead;
use linux_net_bindings::gen_if;
use linux_net_bindings::gen_if_tun;
use linux_net_bindings::tun_set_iff;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::io::Write;
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
    #[error("TAP name conversion to C string failed")]
    TapNameConversion(#[source] std::ffi::NulError),
}

/// Structure corresponding to a TAP interface.
#[derive(Debug)]
pub struct Tap {
    tap: File,
}

impl Tap {
    pub fn new(name: &str) -> Result<Self, Error> {
        let tap = Self::open_tap_interface(name)?;
        Ok(Self { tap })
    }

    fn open_tap_interface(tap_name: &str) -> Result<File, Error> {
        // Open the TUN/TAP interface.
        //
        // - Packets received from this TAP interface (i.e., fom host's network)
        //  will be drained using a nonblocking read() loop, then will be sent to
        //  the guest.
        //
        // - A best effort will be made to send to host's network the packets
        //  received from the guest, by using a nonblocking write() loop on this
        //  TAP interface.
        //
        // RX and/or TX packets might get lost when queues are full, as they get
        // lost sometimes on a physical NIC.
        let tap_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(Error::OpenTunFailed)?;

        // Set TAP interface attributes.
        let mut ifreq: gen_if::ifreq = Default::default();

        let tap_name_cstr = CString::new(tap_name.as_bytes()).map_err(Error::TapNameConversion)?;
        let tap_name_bytes = tap_name_cstr.into_bytes_with_nul();
        let tap_name_length = tap_name_bytes.len();

        // SAFETY: the ifr_ifrn union has a single member, and using
        // ifr_ifrn is consistent with issuing the TUNSETIFF ioctl below.
        let name_slice = unsafe { ifreq.ifr_ifrn.ifrn_name.as_mut() };

        if name_slice.len() < tap_name_length {
            Err(Error::TapNameTooLong(tap_name_length))
        } else {
            for i in 0..tap_name_length {
                name_slice[i] = tap_name_bytes[i] as libc::c_char;
            }
            ifreq.ifr_ifru.ifru_flags = (gen_if_tun::IFF_TAP | gen_if_tun::IFF_NO_PI) as c_short;

            // SAFETY: calling the ioctl according to implementation requirements.
            unsafe {
                tun_set_iff(tap_file.as_raw_fd(), &ifreq)
                    .map_err(|_e| Error::SetTapAttributes(io::Error::last_os_error()))?;
            };
            Ok(tap_file)
        }
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

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
