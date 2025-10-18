// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Windows FFI
#![cfg_attr(windows, expect(unsafe_code))]

use std::io;

use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineError;
use tpm_lib::TpmEngineHelper;

#[cfg(unix)]
use unix::Tpm as TpmInner;
#[cfg(windows)]
use windows::Tpm as TpmInner;

/// Simple cross-platform TPM access (Linux: /dev/tpmrm0|/dev/tpm0, Windows: TBS).
/// Blocking, minimal, not thread-safe across simultaneous transmit calls (uses Mutex).
///
/// Linux notes:
///   Prefer the resource manager device (/dev/tpmrm0) when available.
///   Transmit writes the full command then reads the TPM2 header (10 bytes) to learn total size.
///
/// Windows notes:
///   Uses TBS (TPM Base Services). Link with tbs.dll (implicit).
///   Requires the tbs development headers at build time only for reference; here we redefine what is needed.
pub struct Tpm {
    inner: Inner,
}

/// Low-level TPM transport abstraction.
pub trait RawTpm {
    fn transmit_raw(&self, command: &[u8]) -> io::Result<Vec<u8>>;
}

enum Inner {
    #[cfg(unix)]
    Unix(TpmInner),
    #[cfg(windows)]
    Windows(TpmInner),
}

impl Tpm {
    /// Open default TPM device / context.
    pub fn open() -> io::Result<Self> {
        #[cfg(unix)]
        {
            TpmInner::open().map(|u| Tpm {
                inner: Inner::Unix(u),
            })
        }
        #[cfg(windows)]
        {
            TpmInner::open().map(|w| Tpm {
                inner: Inner::Windows(w),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(io::ErrorKind::Other, "Unsupported platform"))
        }
    }

    /// Transmit a TPM command buffer and return the full response.
    /// Command must already contain a valid TPM header with correct length.
    pub fn transmit(&self, command: &[u8]) -> io::Result<Vec<u8>> {
        if command.len() < 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Command too short",
            ));
        }

        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(u) => u.transmit(command),
            #[cfg(windows)]
            Inner::Windows(w) => w.transmit(command),
        }
    }

    /// Consume this TPM handle and return an engine adapter implementing [`TpmEngine`].
    pub fn into_engine(self) -> DriverTpmEngine<Self>
    where
        Self: Sized + RawTpm + Send,
    {
        DriverTpmEngine::new(self)
    }

    /// Consume this TPM handle and construct a [`TpmEngineHelper`] backed by the device engine.
    pub fn into_engine_helper(self) -> TpmEngineHelper<DriverTpmEngine<Self>>
    where
        Self: Sized + RawTpm + Send,
    {
        TpmEngineHelper::new(self.into_engine())
    }
}

impl RawTpm for Tpm {
    fn transmit_raw(&self, command: &[u8]) -> io::Result<Vec<u8>> {
        self.transmit(command)
    }
}

/// Adapter that lets a [`RawTpm`] implementation satisfy the [`TpmEngine`] trait.
pub struct DriverTpmEngine<T> {
    tpm: T,
}

impl<T> DriverTpmEngine<T> {
    pub fn new(tpm: T) -> Self {
        Self { tpm }
    }
}

impl<T> TpmEngine for DriverTpmEngine<T>
where
    T: RawTpm + Send,
{
    fn execute_command(
        &mut self,
        command: &mut [u8],
        response: &mut [u8],
    ) -> Result<(), TpmEngineError> {
        if command.len() < 10 {
            return Err(TpmEngineError::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TPM command shorter than header",
            )));
        }

        let reply = self
            .tpm
            .transmit_raw(command)
            .map_err(TpmEngineError::new)?;

        if reply.len() > response.len() {
            return Err(TpmEngineError::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "TPM response length {} exceeds reply buffer {}",
                    reply.len(),
                    response.len()
                ),
            )));
        }

        let filled = reply.len();
        response[..filled].copy_from_slice(&reply);
        response[filled..].fill(0);

        Ok(())
    }
}

#[cfg(unix)]
mod unix {
    use parking_lot::Mutex;
    use std::fs::OpenOptions;
    use std::io;
    use std::io::Read;
    use std::io::Write;

    /// Struct that holds the TPM device handle.
    pub struct Tpm {
        file: Mutex<std::fs::File>,
    }

    impl Tpm {
        /// Open a TPM device.
        pub fn open() -> io::Result<Self> {
            let candidates = ["/dev/tpmrm0", "/dev/tpm0"];
            let mut last_err = None;
            for path in candidates {
                match OpenOptions::new().read(true).write(true).open(path) {
                    Ok(f) => {
                        return Ok(Tpm {
                            file: Mutex::new(f),
                        });
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(last_err
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No TPM device")))
        }

        /// Send a command to TPM.
        pub fn transmit(&self, command: &[u8]) -> io::Result<Vec<u8>> {
            let mut f = self.file.lock();

            // Write full command
            f.write_all(command)?;

            // Read TPM header (10 bytes)
            let mut header = [0u8; 10];
            f.read_exact(&mut header)?;

            // Parse total response size (bytes 2..6 big-endian)
            let size = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
            if size < 10 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid TPM response length",
                ));
            }
            let mut resp = Vec::with_capacity(size);
            resp.extend_from_slice(&header);

            let remaining = size - 10;
            if remaining > 0 {
                let mut rest = vec![0u8; remaining];
                f.read_exact(&mut rest)?;
                resp.extend_from_slice(&rest);
            }

            Ok(resp)
        }
    }
}

#[cfg(windows)]
mod windows {
    use parking_lot::Mutex;
    use std::io;

    /// Struct that holds the TBS handle and the lock.
    pub struct Tpm {
        handle: u32, // TBS_HCONTEXT
        lock: Mutex<()>,
    }

    impl Tpm {
        /// Open a TPM device.
        pub fn open() -> io::Result<Self> {
            let params2 = win_ffi::TBS_CONTEXT_PARAMS2 {
                version: win_ffi::TPM_VERSION_20, // required for PARAMS2
                Anonymous: win_ffi::TBS_CONTEXT_PARAMS2_FLAGS { asUINT32: 0x6 }, // includeTpm12 | includeTpm20
            };

            let mut handle: u32 = 0;
            // SAFETY: Make an FFI call.
            let rc = unsafe {
                win_ffi::Tbsi_Context_Create(
                    std::ptr::from_ref(&params2),
                    std::ptr::from_mut(&mut handle),
                )
            };
            if rc == win_ffi::TBS_SUCCESS {
                return Ok(Tpm {
                    handle,
                    lock: Mutex::new(()),
                });
            }

            if rc == 0x8028_400F {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "TPM not found (TBS_E_TPM_NOT_FOUND) rc=0x{rc:08x}. System reports TPM present so this may indicate a TBS access restriction (service state, policy) or a virtualization layer issue."
                    ),
                ));
            }
            Err(io::Error::other(format!(
                "Tbsi_Context_Create failed rc=0x{rc:08x}"
            )))
        }

        /// Send a command to TPM.
        pub fn transmit(&self, command: &[u8]) -> io::Result<Vec<u8>> {
            if command.len() > u32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Command too large",
                ));
            }
            let _g = self.lock.lock();
            let mut buf = vec![0u8; 8192];
            let mut out_len: u32 = buf.len() as u32;
            // SAFETY: Make an FFI call.
            let rc = unsafe {
                win_ffi::Tbsip_Submit_Command(
                    self.handle,
                    win_ffi::TBS_COMMAND_LOCALITY_ZERO,
                    win_ffi::TBS_COMMAND_PRIORITY_NORMAL,
                    command.as_ptr(),
                    command.len() as u32,
                    buf.as_mut_ptr(),
                    std::ptr::from_mut::<u32>(&mut out_len),
                )
            };

            if rc != win_ffi::TBS_SUCCESS {
                return Err(io::Error::other(format!(
                    "Tbsip_Submit_Command failed: 0x{rc:08x}"
                )));
            }

            buf.truncate(out_len as usize);
            if buf.len() < 10 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Response too short",
                ));
            }
            let declared = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
            if declared != buf.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Response length mismatch: declared {declared} actual {}",
                        buf.len()
                    ),
                ));
            }
            Ok(buf)
        }
    }

    impl Drop for Tpm {
        fn drop(&mut self) {
            // SAFETY: Make an FFI call.
            unsafe { win_ffi::Tbsip_Context_Close(self.handle) };
        }
    }

    /// Minimal subset of TBS FFI we need (manual because windows crate does not currently expose TBS APIs).
    mod win_ffi {
        pub const TBS_SUCCESS: u32 = 0;
        pub const TPM_VERSION_20: u32 = 2;
        pub const TBS_COMMAND_LOCALITY_ZERO: u32 = 0;
        pub const TBS_COMMAND_PRIORITY_NORMAL: u32 = 100;

        /// Allow non-snake / camel case naming that matches the Windows SDK for FFI correctness.
        #[allow(non_snake_case)]
        #[repr(C)]
        pub union TBS_CONTEXT_PARAMS2_FLAGS {
            pub asUINT32: u32, // bit 0: requestRaw, bit 1: includeTpm12, bit 2: includeTpm20
        }

        #[repr(C)]
        #[allow(non_camel_case_types)]
        #[allow(non_snake_case)]
        pub struct TBS_CONTEXT_PARAMS2 {
            pub version: u32, // must be TPM_VERSION_20 for PARAMS2
            pub Anonymous: TBS_CONTEXT_PARAMS2_FLAGS,
        }

        #[link(name = "tbs")]
        unsafe extern "system" {
            pub fn Tbsi_Context_Create(params: *const TBS_CONTEXT_PARAMS2, handle: *mut u32)
            -> u32;
            pub fn Tbsip_Context_Close(handle: u32);
            pub fn Tbsip_Submit_Command(
                handle: u32,
                locality: u32,
                priority: u32,
                commandBuffer: *const u8,
                commandBufferSize: u32,
                resultBuffer: *mut u8,
                resultBufferSize: *mut u32,
            ) -> u32;
        }
    }
}
