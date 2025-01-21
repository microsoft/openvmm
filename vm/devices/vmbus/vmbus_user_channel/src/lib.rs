// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for a vmbus channel client via the /dev/uioX devices.
//!
//! This mechanism works as follows. The `uio_vmbus_client` kernel driver can be
//! configured to bind to devices of a given vmbus device ID by writing the
//! device ID to to `/sys/bus/vmbus/uio_vmbus_client/new_id`. Then, for each vmbus
//! channel with that device ID, the driver will create a device named
//! `/dev/uioX`.
//!
//! When this device is opened, the driver will allocate a ring buffer and open
//! the corresponding vmbus channel. The channel can then be controlled in the
//! following ways:
//!
//! * Ring buffer memory can be accessed by using `mmap` to map the memory into
//!   user mode.
//!
//! * The host can be signaled by using `write` to write an 4-byte non-zero
//!   value. This is the same mechanism as `eventfd`.
//!
//! * The guest can wait for a signal by using `read` to read an 4-byte value.
//!   The file can be marked non-blocking, in which case the read will fail with
//!   `EAGAIN` if there is no signal. The guest can additionally use the
//!   kernel's `poll` infrastructure to wait for a signal to be available by
//!   waiting for `POLLIN` readiness.
//!
//! Currently there is no mechanism to discover that the channel has been
//! revoked.
//!

#![cfg(unix)]
#![warn(missing_docs)]
#![forbid(unsafe_code)]

use filepath::FilePath;
use guid::Guid;
use pal_async::driver::Driver;
use pal_async::wait::PolledWait;
use parking_lot::Mutex;
use safeatomic::AtomicSliceOps;
use sparse_mmap::SparseMapping;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::task::ready;
use thiserror::Error;
use vmbus_async::pipe::BytePipe;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::RingMem;
use zerocopy::IntoBytes;

/// Ring buffer memory backed by a memory mapped channel.
#[derive(Debug)]
pub struct MappedRingMem {
    mapping: Arc<SparseMapping>,
    offset: usize,
    len: usize,
}

const CONTROL_SIZE: usize = 0x1000;
// These are currently hard-coded in the kernel driver. If this becomes
// configurable in the future, then query the device to determine these
// parameters.
const OUT_RING_SIZE: usize = 0x10000;
const IN_RING_SIZE: usize = 0x10000;

impl RingMem for MappedRingMem {
    fn control(&self) -> &[std::sync::atomic::AtomicU32; vmbus_ring::CONTROL_WORD_COUNT] {
        self.mapping
            .atomic_slice(self.offset, CONTROL_SIZE)
            .as_atomic_slice()
            .unwrap()[..vmbus_ring::CONTROL_WORD_COUNT]
            .try_into()
            .unwrap()
    }

    fn read_at(&self, mut addr: usize, data: &mut [u8]) {
        debug_assert!(addr + data.len() <= 2 * self.len);
        if addr > self.len() {
            addr -= self.len();
        }
        if addr + data.len() <= self.len() {
            self.mapping
                .read_at(self.offset + CONTROL_SIZE + addr, data)
                .unwrap();
        } else {
            let (first, last) = data.split_at_mut(self.len() - addr);
            self.mapping
                .read_at(self.offset + CONTROL_SIZE + addr, first)
                .unwrap();
            self.mapping
                .read_at(self.offset + CONTROL_SIZE, last)
                .unwrap();
        }
    }

    fn write_at(&self, mut addr: usize, data: &[u8]) {
        debug_assert!(addr + data.len() <= 2 * self.len);
        if addr > self.len() {
            addr -= self.len();
        }
        if addr + data.len() <= self.len() {
            self.mapping
                .write_at(self.offset + CONTROL_SIZE + addr, data)
                .unwrap();
        } else {
            let (first, last) = data.split_at(self.len() - addr);
            self.mapping
                .write_at(self.offset + CONTROL_SIZE + addr, first)
                .unwrap();
            self.mapping
                .write_at(self.offset + CONTROL_SIZE, last)
                .unwrap();
        }
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[derive(Debug, Error)]
enum ErrorInner {
    #[error("couldn't find uio device")]
    Exist(#[source] std::io::Error),
    #[error("failed to open file")]
    Open(#[source] std::io::Error),
    #[error("failed to mmap")]
    Mmap(#[source] std::io::Error),
    #[error("ring buffer error")]
    Ring(#[source] vmbus_ring::Error),
    #[error("vmbus pipe error")]
    Pipe(#[source] std::io::Error),
    #[error("driver error")]
    Driver(#[source] std::io::Error),
}

/// An error connecting to the vmbus channel.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(ErrorInner);

impl<T: Into<ErrorInner>> From<T> for Error {
    fn from(t: T) -> Self {
        Self(t.into())
    }
}

/// Opens the UIO device for passing to [`channel`].
pub fn open_uio_device(instance_id: &Guid) -> Result<File, Error> {
    let paths = fs_err::read_dir(format!("/sys/bus/vmbus/devices/{instance_id}/uio"))
        .map_err(ErrorInner::Exist)?;

    let uio_path = paths
        .last()
        .unwrap_or_else(|| Err(std::io::ErrorKind::NotFound.into()))
        .map_err(ErrorInner::Exist)?;

    let uio_dev_path = Path::new("/dev").join(uio_path.file_name());
    tracing::debug!(
        dev_path = %uio_dev_path.display(),
        %instance_id,
        "opening device"
    );

    let file = fs_err::OpenOptions::new()
        .read(true)
        .write(true)
        .open(uio_dev_path)
        .map_err(ErrorInner::Open)?;

    Ok(file.into())
}

/// Opens a channel with a file from [`open_uio_device`].
pub fn channel(
    driver: &(impl Driver + ?Sized),
    file: File,
) -> Result<RawAsyncChannel<MappedRingMem>, Error> {
    let total_mapping_size = CONTROL_SIZE + IN_RING_SIZE + CONTROL_SIZE + OUT_RING_SIZE;

    let mapping = Arc::new(SparseMapping::new(total_mapping_size).map_err(ErrorInner::Mmap)?);

    // Double map the data portion of the ring buffers so that a packet spanning
    // the end of the ring buffer can be read linearly in VA space.
    let mapping_offset = 0;
    let len = CONTROL_SIZE + OUT_RING_SIZE + CONTROL_SIZE + IN_RING_SIZE;

    mapping
        .map_file(mapping_offset, len, &file, 0_u64, true)
        .map_err(ErrorInner::Mmap)?;

    let file = Arc::new(file);
    // UIO uses a 4-byte read to consume an interrupt.
    let wait = PolledWait::new_with_size(driver, file.clone(), 4).map_err(ErrorInner::Driver)?;
    let signal = UioSignal {
        wait: Mutex::new(wait),
        file,
    };

    let out_mem = MappedRingMem {
        mapping: mapping.clone(),
        offset: 0,
        len: OUT_RING_SIZE,
    };
    let out_ring = OutgoingRing::new(out_mem).map_err(ErrorInner::Ring)?;
    let in_mem = MappedRingMem {
        mapping,
        offset: CONTROL_SIZE + OUT_RING_SIZE,
        len: IN_RING_SIZE,
    };
    let in_ring = IncomingRing::new(in_mem).map_err(ErrorInner::Ring)?;

    let channel = RawAsyncChannel {
        in_ring,
        out_ring,
        signal: Box::new(signal),
    };

    Ok(channel)
}

struct UioSignal {
    file: Arc<File>,
    wait: Mutex<PolledWait<Arc<File>>>,
}

impl UioSignal {
    /// Attempt to get the interface and instance IDs for the channel.
    fn ids(&self) -> Option<(String, String)> {
        let path = self.file.path().ok()?;
        let sysfs = Path::new("/sys/bus/uio").join(path.file_name()?);
        let interface_id = fs_err::read_to_string(sysfs.join("device/class_id")).ok()?;
        let instance_id = fs_err::read_to_string(sysfs.join("device/device_id")).ok()?;
        Some((interface_id, instance_id))
    }
}

impl SignalVmbusChannel for UioSignal {
    fn signal_remote(&self) {
        // UIO uses 4-byte writes to signal the host and 4-byte reads to consume
        // a signal from the host. Use `as_bytes` to get an appropriately
        // aligned buffer.
        let n = self.file.as_ref().write(1u32.as_bytes()).unwrap();
        assert_eq!(n, 4);
    }

    fn poll_for_signal(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), vmbus_channel::ChannelClosed>> {
        match ready!(self.wait.lock().poll_wait(cx)) {
            Ok(()) => Ok(()),
            Err(err) => {
                let (interface_id, instance_id) = self.ids().unzip();
                let interface_id = interface_id.as_ref().map(|s| s.trim_end());
                let interface_id = interface_id.as_ref().map(|s| s.trim_end());
                if err.raw_os_error() == Some(libc::EIO) {
                    tracing::info!(interface_id, instance_id, "vmbus channel revoked");
                } else {
                    tracing::error!(
                        interface_id,
                        instance_id,
                        error = &err as &dyn std::error::Error,
                        "unexpected uio error, treating as revoked channel"
                    )
                }
                Err(vmbus_channel::ChannelClosed)
            }
        }
        .into()
    }
}

/// Opens a byte pipe for the channel with a file from [`open_uio_device`].
pub fn byte_pipe(
    driver: &(impl Driver + ?Sized),
    file: File,
) -> Result<BytePipe<MappedRingMem>, Error> {
    let channel = channel(driver, file)?;
    let pipe = BytePipe::new(channel).map_err(ErrorInner::Pipe)?;
    Ok(pipe)
}

/// Opens a message pipe for the channel with a file from [`open_uio_device`].
pub fn message_pipe(
    driver: &(impl Driver + ?Sized),
    file: File,
) -> Result<MessagePipe<MappedRingMem>, Error> {
    let channel = channel(driver, file)?;
    let pipe = MessagePipe::new(channel).map_err(ErrorInner::Pipe)?;
    Ok(pipe)
}
