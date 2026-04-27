// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native API cross-validation smoke tests.
//!
//! These tests exercise the first interaction between the Rust VHDX parser
//! and the Windows native VHD stack. They are deliberately limited in scope
//! to surface format-level bugs that need to be diagnosed and fixed before
//! writing a full test suite.
//!
//! **All tests are gated with `#[cfg(windows)]`.**
//!
//! ## Format bugs discovered and fixed
//!
//! (Updated as bugs are found during cross-validation.)

#![cfg(windows)]
// UNSAFETY: Windows FFI calls for virtual disk APIs and raw disk I/O.
#![expect(unsafe_code)]

use pal_async::DefaultDriver;
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::io;
use std::path::Path;
use std::sync::Arc;
use vhdx::AsyncFile;
use vhdx::ReadRange;
use vhdx::TrimMode;
use vhdx::TrimRequest;
use vhdx::WriteRange;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::Storage::FileSystem::WriteFile;
use windows::Win32::Storage::Vhd::ATTACH_VIRTUAL_DISK_FLAG;
use windows::Win32::Storage::Vhd::ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER;
use windows::Win32::Storage::Vhd::ATTACH_VIRTUAL_DISK_FLAG_NO_LOCAL_HOST;
use windows::Win32::Storage::Vhd::AttachVirtualDisk;
use windows::Win32::Storage::Vhd::CREATE_VIRTUAL_DISK_FLAG_NONE;
use windows::Win32::Storage::Vhd::CREATE_VIRTUAL_DISK_PARAMETERS;
use windows::Win32::Storage::Vhd::CREATE_VIRTUAL_DISK_VERSION_2;
use windows::Win32::Storage::Vhd::CreateVirtualDisk;
use windows::Win32::Storage::Vhd::DetachVirtualDisk;
use windows::Win32::Storage::Vhd::OPEN_VIRTUAL_DISK_FLAG_NONE;
use windows::Win32::Storage::Vhd::OPEN_VIRTUAL_DISK_PARAMETERS;
use windows::Win32::Storage::Vhd::OPEN_VIRTUAL_DISK_VERSION_2;
use windows::Win32::Storage::Vhd::OpenVirtualDisk;
use windows::Win32::Storage::Vhd::VIRTUAL_DISK_ACCESS_MASK;
use windows::Win32::Storage::Vhd::VIRTUAL_STORAGE_TYPE;
use windows::Win32::System::IO::GetOverlappedResult;
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

// ---------------------------------------------------------------------
// StdFile — blocking AsyncFile adapter for integration tests
// ---------------------------------------------------------------------

/// Blocking `AsyncFile` impl backed by `std::fs::File`.
/// Suitable for tests only — all operations block the current thread.
struct StdFile {
    file: Mutex<std::fs::File>,
}

impl StdFile {
    fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn create(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl AsyncFile for StdFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, io::Error> {
        use std::io::Read;
        use std::io::Seek;
        use std::io::SeekFrom;
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf.as_mut())?;
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), io::Error> {
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::io::Write;
        let buf = buf.borrow();
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(buf.as_ref())
    }

    async fn flush(&self) -> Result<(), io::Error> {
        use std::io::Write;
        let mut file = self.file.lock();
        file.flush()
    }

    async fn file_size(&self) -> Result<u64, io::Error> {
        let file = self.file.lock();
        file.metadata().map(|m| m.len())
    }

    async fn set_file_size(&self, size: u64) -> Result<(), io::Error> {
        let file = self.file.lock();
        file.set_len(size)
    }
}

// ---------------------------------------------------------------------
// Windows Virtual Disk Type Constants
// ---------------------------------------------------------------------

const VIRTUAL_STORAGE_TYPE_DEVICE_VHDX: u32 = 3;

// Microsoft vendor GUID: {EC984AEC-A0F9-47e9-901F-71415A66345B}
const VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT: windows::core::GUID = windows::core::GUID {
    data1: 0xEC984AEC,
    data2: 0xA0F9,
    data3: 0x47e9,
    data4: [0x90, 0x1F, 0x71, 0x41, 0x5A, 0x66, 0x34, 0x5B],
};

// ---------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---------------------------------------------------------------------
// NativeVhdx — RAII wrapper around Windows virtual disk APIs
// ---------------------------------------------------------------------

struct NativeVhdx {
    handle: HANDLE,
    attached: bool,
}

impl NativeVhdx {
    /// Create a new dynamic VHDX via CreateVirtualDisk.
    fn create_dynamic(path: &Path, size_bytes: u64, block_size: u32, sector_size: u32) -> Self {
        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };

        let wide = to_wide(path);

        let mut params = CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: CREATE_VIRTUAL_DISK_VERSION_2,
            ..Default::default()
        };
        params.Anonymous.Version2.MaximumSize = size_bytes;
        params.Anonymous.Version2.BlockSizeInBytes = block_size;
        params.Anonymous.Version2.SectorSizeInBytes = sector_size;

        let mut handle = HANDLE::default();

        // SAFETY: All parameters are correctly initialized, wide path is
        // null-terminated, and handle is written by the API on success.
        let result = unsafe {
            CreateVirtualDisk(
                &storage_type,
                PCWSTR(wide.as_ptr()),
                VIRTUAL_DISK_ACCESS_MASK(0),
                None,
                CREATE_VIRTUAL_DISK_FLAG_NONE,
                0,
                &params,
                None,
                &mut handle,
            )
        };
        assert!(result.is_ok(), "CreateVirtualDisk failed: {result:?}");

        NativeVhdx {
            handle,
            attached: false,
        }
    }

    /// Open an existing VHDX via OpenVirtualDisk.
    fn open(path: &Path, _read_only: bool) -> Self {
        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };

        let wide = to_wide(path);

        let params = OPEN_VIRTUAL_DISK_PARAMETERS {
            Version: OPEN_VIRTUAL_DISK_VERSION_2,
            ..Default::default()
        };

        let mut handle = HANDLE::default();

        // SAFETY: All parameters are correctly initialized, wide path is
        // null-terminated, and handle is written by the API on success.
        let result = unsafe {
            OpenVirtualDisk(
                &storage_type,
                PCWSTR(wide.as_ptr()),
                VIRTUAL_DISK_ACCESS_MASK(0),
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                Some(&params),
                &mut handle,
            )
        };
        assert!(result.is_ok(), "OpenVirtualDisk failed: {result:?}");

        NativeVhdx {
            handle,
            attached: false,
        }
    }

    /// Attach with NO_LOCAL_HOST for raw byte-level I/O.
    /// With NO_LOCAL_HOST, no PhysicalDrive device is surfaced — instead,
    /// ReadFile/WriteFile work directly on the virtual disk handle.
    fn attach_raw(&mut self) -> RawDiskHandle {
        let flags = ATTACH_VIRTUAL_DISK_FLAG(
            ATTACH_VIRTUAL_DISK_FLAG_NO_LOCAL_HOST.0 | ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER.0,
        );

        // SAFETY: Handle is valid (from Create/OpenVirtualDisk). Flags are valid.
        let result = unsafe { AttachVirtualDisk(self.handle, None, flags, 0, None, None) };
        assert!(result.is_ok(), "AttachVirtualDisk failed: {result:?}");
        self.attached = true;

        // With NO_LOCAL_HOST the virtual disk handle itself supports
        // ReadFile/WriteFile at virtual-disk offsets. No PhysicalDrive path.
        RawDiskHandle {
            handle: self.handle,
            owned: false,
        }
    }
}

impl Drop for NativeVhdx {
    fn drop(&mut self) {
        if self.attached {
            // SAFETY: Handle is valid and was successfully attached.
            let _ = unsafe { DetachVirtualDisk(self.handle, Default::default(), 0) };
            self.attached = false;
        }
        if !self.handle.is_invalid() {
            // SAFETY: Handle is valid (from Create/OpenVirtualDisk).
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------
// RawDiskHandle — read/write at byte offsets on attached virtual disk
// ---------------------------------------------------------------------

struct RawDiskHandle {
    handle: HANDLE,
    /// Whether this handle is owned (should be closed on drop).
    /// When borrowed from NativeVhdx (NO_LOCAL_HOST attach), this is false.
    owned: bool,
}

impl RawDiskHandle {
    /// Read `buf.len()` bytes from the raw disk at the given byte offset.
    /// Offset and length must be sector-aligned (multiples of 512).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: Creating a manual-reset event for overlapped I/O.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        let mut overlapped: OVERLAPPED = Default::default();
        overlapped.Anonymous.Anonymous.Offset = (offset & 0xFFFF_FFFF) as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped.hEvent = event;

        let mut bytes_read = 0u32;
        // SAFETY: Handle is valid, buf is valid for buf.len() bytes,
        // overlapped is correctly initialized with event and offset.
        let result = unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut bytes_read),
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_IO_PENDING.into() => {
                // ERROR_IO_PENDING — wait for completion.
                // SAFETY: Handle and overlapped are valid; bWait=true blocks.
                unsafe { GetOverlappedResult(self.handle, &overlapped, &mut bytes_read, true) }
                    .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
            }
            Err(e) => {
                // SAFETY: Event handle is valid.
                let _ = unsafe { CloseHandle(event) };
                return Err(io::Error::from_raw_os_error(e.code().0));
            }
        }
        // SAFETY: Event handle is valid.
        let _ = unsafe { CloseHandle(event) };
        Ok(bytes_read as usize)
    }

    /// Write `data.len()` bytes to the raw disk at the given byte offset.
    /// Offset and length must be sector-aligned (multiples of 512).
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<usize> {
        // SAFETY: Creating a manual-reset event for overlapped I/O.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        let mut overlapped: OVERLAPPED = Default::default();
        overlapped.Anonymous.Anonymous.Offset = (offset & 0xFFFF_FFFF) as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped.hEvent = event;

        let mut bytes_written = 0u32;
        // SAFETY: Handle is valid, data is valid for data.len() bytes,
        // overlapped is correctly initialized with event and offset.
        let result = unsafe {
            WriteFile(
                self.handle,
                Some(data),
                Some(&mut bytes_written),
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_IO_PENDING.into() => {
                // ERROR_IO_PENDING — wait for completion.
                // SAFETY: Handle and overlapped are valid; bWait=true blocks.
                unsafe { GetOverlappedResult(self.handle, &overlapped, &mut bytes_written, true) }
                    .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
            }
            Err(e) => {
                // SAFETY: Event handle is valid.
                let _ = unsafe { CloseHandle(event) };
                return Err(io::Error::from_raw_os_error(e.code().0));
            }
        }
        // SAFETY: Event handle is valid.
        let _ = unsafe { CloseHandle(event) };
        Ok(bytes_written as usize)
    }
}

impl Drop for RawDiskHandle {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_invalid() {
            // SAFETY: Handle is valid and owned by this struct.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------
// RustVhdx — helper wrapping the Rust VHDX API for test scenarios
// ---------------------------------------------------------------------

struct RustVhdx {
    vhdx: vhdx::VhdxFile<StdFile>,
    /// Separate file handle for data I/O (shared backing path).
    io_file: Arc<StdFile>,
}

impl RustVhdx {
    async fn create(path: &Path, disk_size: u64, block_size: u32, driver: &DefaultDriver) -> Self {
        let file = StdFile::create(path).expect("create backing file");
        let mut params = vhdx::CreateParams {
            disk_size,
            block_size,
            ..Default::default()
        };
        vhdx::create(&file, &mut params).await.expect("vhdx create");
        drop(file);

        // Re-open for use with log task.
        Self::open(path, false, Some(driver)).await
    }

    async fn open(path: &Path, read_only: bool, driver: Option<&DefaultDriver>) -> Self {
        let file = StdFile::open(path, read_only).expect("open backing file");
        let io_file = Arc::new(StdFile::open(path, read_only).expect("open io file"));
        let vhdx = if read_only {
            vhdx::VhdxFile::open(file)
                .read_only()
                .await
                .expect("vhdx open")
        } else {
            let driver = driver.expect("writable open requires a driver/spawner");
            vhdx::VhdxFile::open(file)
                .writable(driver)
                .await
                .expect("vhdx open_writable")
        };
        RustVhdx { vhdx, io_file }
    }

    /// Read data at a virtual offset. Returns a Vec<u8> of `len` bytes.
    async fn read_data(&self, offset: u64, len: u32) -> Vec<u8> {
        let mut ranges = Vec::new();
        let guard = self
            .vhdx
            .resolve_read(offset, len, &mut ranges)
            .await
            .expect("resolve_read");

        let mut result = vec![0u8; len as usize];

        for range in &ranges {
            match range {
                ReadRange::Data {
                    guest_offset,
                    length,
                    file_offset,
                } => {
                    let buf_offset = (*guest_offset - offset) as usize;
                    let buf_len = *length as usize;
                    self.io_file
                        .read_at(*file_offset, &mut result[buf_offset..buf_offset + buf_len])
                        .await
                        .expect("read data from file");
                }
                ReadRange::Zero { .. } | ReadRange::Unmapped { .. } => {
                    // Already zero-initialized.
                }
            }
        }

        drop(guard);
        result
    }

    /// Write data at a virtual offset.
    async fn write_data(&self, offset: u64, data: &[u8]) {
        let mut ranges = Vec::new();
        let guard = self
            .vhdx
            .resolve_write(offset, data.len() as u32, &mut ranges)
            .await
            .expect("resolve_write");

        for range in &ranges {
            match range {
                WriteRange::Data {
                    guest_offset,
                    length,
                    file_offset,
                } => {
                    let buf_offset = (*guest_offset - offset) as usize;
                    let buf_len = *length as usize;
                    self.io_file
                        .write_at(*file_offset, &data[buf_offset..buf_offset + buf_len])
                        .await
                        .expect("write data to file");
                }
                WriteRange::Zero {
                    file_offset,
                    length,
                } => {
                    let zeros = vec![0u8; *length as usize];
                    self.io_file
                        .write_at(*file_offset, &zeros)
                        .await
                        .expect("zero-fill file range");
                }
            }
        }

        guard.complete().await.expect("write complete");
    }

    /// Flush the VHDX file.
    async fn flush(&self) {
        self.vhdx.flush().await.expect("flush");
    }

    /// Trim a range of the virtual disk.
    async fn trim_range(&self, offset: u64, length: u64) {
        self.vhdx
            .trim(TrimRequest::new(TrimMode::Zero, offset, length))
            .await
            .expect("trim");
    }

    /// Close the VHDX (consume self).
    async fn close(self) {
        self.vhdx.close().await.expect("close");
    }

    /// Abort (crash) the VHDX — drops without clean close, leaving a dirty log.
    async fn abort(self) {
        self.vhdx.abort().await;
    }
}

// =====================================================================
// Test Data Pattern
// =====================================================================

/// Generate a test pattern for a given offset: the pattern byte
/// is derived from the offset so each location has unique data.
fn test_pattern(offset: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((offset as usize + i) % 251) as u8) // prime modulus avoids power-of-2 alignment
        .collect()
}

// =====================================================================
// Test Cases
// =====================================================================

/// Test 1: Native-Create → Rust-Open (Metadata Check)
///
/// Native creates a dynamic VHDX (1 GiB) → close → Rust opens → verify
/// disk geometry matches.
#[pal_async::async_test]
async fn native_create_rust_open_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    // Native create: 1 GiB, default block/sector sizes (pass 0 for defaults).
    {
        let _native = NativeVhdx::create_dynamic(&vhdx_path, 1024 * 1024 * 1024, 0, 0);
        // Drop closes the handle.
    }

    // Rust open and verify metadata.
    let rust = RustVhdx::open(&vhdx_path, true, None).await;

    // Native defaults: 1 GiB disk, typically 32 MiB block size, 512 sector sizes.
    assert_eq!(rust.vhdx.disk_size(), 1024 * 1024 * 1024, "disk_size");
    // The native default block size is typically 32 MiB, but may vary.
    // Just assert it's a power of 2 and > 0.
    let block_size = rust.vhdx.block_size();
    assert!(block_size > 0 && block_size.is_power_of_two(), "block_size");
    // Sector sizes: native defaults to 512 logical, 4096 physical.
    assert_eq!(rust.vhdx.logical_sector_size(), 512, "logical_sector_size");
    assert_eq!(
        rust.vhdx.physical_sector_size(),
        4096,
        "physical_sector_size"
    );

    rust.close().await;
}

/// Test 2: Rust-Create → Native-Open (Open Succeeds)
///
/// Rust creates a dynamic VHDX (1 GiB) → close → native OpenVirtualDisk
/// succeeds.
#[pal_async::async_test]
async fn rust_create_native_open(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    // Rust create: 1 GiB, 2 MiB block size (Rust default), 512-byte sectors.
    {
        let rust = RustVhdx::create(&vhdx_path, 1024 * 1024 * 1024, 0, &driver).await;
        rust.close().await;
    }

    // Native open — this is the most likely test to fail.
    let _native = NativeVhdx::open(&vhdx_path, true);
    // If we get here, the native stack accepted the Rust-created file.
}

/// Test 3: Rust-Create → Native-Attach → Raw-Read Zeros
///
/// Rust creates a small dynamic VHDX (4 MiB, 2 MiB blocks) → close →
/// native opens → attach → raw-read first sector → verify all zeros.
#[pal_async::async_test]
async fn rust_create_native_attach_read_zeros(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    // Rust create: 4 MiB disk, 2 MiB block size.
    {
        let rust = RustVhdx::create(&vhdx_path, 4 * 1024 * 1024, 2 * 1024 * 1024, &driver).await;
        rust.flush().await;
        rust.close().await;
    }

    // Native open + attach.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    // Read the first sector (512 bytes) at offset 0.
    let mut buf = vec![0xCCu8; 512];
    let bytes_read = raw.read_at(0, &mut buf).expect("raw read at offset 0");
    assert_eq!(bytes_read, 512, "expected 512 bytes read");

    // A freshly-created, never-written VHDX should return all zeros.
    assert!(buf.iter().all(|&b| b == 0), "first sector should be zeros");
}

/// Test 4: Native-Create → Native-Write → Rust-Read (Data)
///
/// Native creates dynamic VHDX (1 GiB, default sizes) → attach → write
/// known patterns at 3 offsets across different blocks → detach → close →
/// Rust opens → reads at each offset → data matches.
#[pal_async::async_test]
async fn native_create_rust_read_data() {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    // Determine block size after native create (typically 32 MiB).
    let block_size: u64;

    // Native create + write.
    {
        let mut native = NativeVhdx::create_dynamic(&vhdx_path, 1024 * 1024 * 1024, 0, 0);
        let raw = native.attach_raw();

        // We need to know the block size to write across blocks.
        // Native defaults to 32 MiB blocks.
        block_size = 32 * 1024 * 1024;

        let offsets = [0u64, block_size, 2 * block_size];
        for &off in &offsets {
            let pattern = test_pattern(off, 512);
            let written = raw.write_at(off, &pattern).expect("native write");
            assert_eq!(written, 512);
        }
        // Drop detaches and closes.
    }

    // Rust open + read + verify.
    let rust = RustVhdx::open(&vhdx_path, true, None).await;

    let offsets = [0u64, block_size, 2 * block_size];
    for &off in &offsets {
        let expected = test_pattern(off, 512);
        let actual = rust.read_data(off, 512).await;
        assert_eq!(actual, expected, "data mismatch at offset {off:#x}");
    }

    rust.close().await;
}

/// Test 5: Native-Create → Rust-Read (Custom 32 MiB Block Size)
///
/// Native creates with explicit 32 MiB block size → Rust opens →
/// `block_size()` == 32 MiB.
#[pal_async::async_test]
async fn native_create_custom_block_size() {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    {
        let _native =
            NativeVhdx::create_dynamic(&vhdx_path, 1024 * 1024 * 1024, 32 * 1024 * 1024, 0);
    }

    let rust = RustVhdx::open(&vhdx_path, true, None).await;
    assert_eq!(
        rust.vhdx.block_size(),
        33554432,
        "block_size should be 32 MiB"
    );
    rust.close().await;
}

/// Test 6: Native-Create → Rust-Read (4K Logical Sector)
///
/// Native creates with 4096 logical sector size → Rust opens →
/// `logical_sector_size()` == 4096.
#[pal_async::async_test]
async fn native_create_4k_sector() {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    {
        let _native = NativeVhdx::create_dynamic(&vhdx_path, 1024 * 1024 * 1024, 0, 4096);
    }

    let rust = RustVhdx::open(&vhdx_path, true, None).await;
    assert_eq!(
        rust.vhdx.logical_sector_size(),
        4096,
        "logical_sector_size should be 4096"
    );
    rust.close().await;
}

/// Test 7: Rust-Create → Native-Read (Data)
///
/// Rust creates + writes data at multiple offsets across block boundaries →
/// flush → close → native opens → attach → raw-read at each offset →
/// data matches.
#[pal_async::async_test]
async fn rust_create_native_read_data(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024; // 2 MiB

    // Rust create + write.
    {
        let rust = RustVhdx::create(&vhdx_path, 32 * 1024 * 1024, block_size as u32, &driver).await;

        // Write to 3 different blocks (blocks 1, 3, 5 — skip block 0 since test 3 uses it).
        let offsets = [block_size, 3 * block_size, 5 * block_size];
        for &off in &offsets {
            let pattern = test_pattern(off, 512);
            rust.write_data(off, &pattern).await;
        }
        rust.flush().await;
        rust.close().await;
    }

    // Native open + attach + read + verify.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    let offsets = [block_size, 3 * block_size, 5 * block_size];
    for &off in &offsets {
        let expected = test_pattern(off, 512);
        let mut buf = vec![0u8; 512];
        let bytes_read = raw.read_at(off, &mut buf).expect("native read");
        assert_eq!(bytes_read, 512);
        assert_eq!(buf, expected, "data mismatch at offset {off:#x}");
    }
}

/// Test 8: Rust-Create → Native-Open (Various Block Sizes)
///
/// Rust creates VHDX files with 2 MiB, 4 MiB, and 32 MiB block sizes →
/// native opens each → open succeeds without error.
#[pal_async::async_test]
async fn rust_create_various_block_sizes(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let block_sizes: &[u32] = &[2 * 1024 * 1024, 4 * 1024 * 1024, 32 * 1024 * 1024];

    for &bs in block_sizes {
        let name = format!("test_bs_{bs}.vhdx");
        let vhdx_path = dir.path().join(&name);

        {
            let rust = RustVhdx::create(&vhdx_path, 64 * 1024 * 1024, bs, &driver).await;
            rust.close().await;
        }

        let _native = NativeVhdx::open(&vhdx_path, true);
        // If we get here, the native stack accepted the file.
    }
}

/// Test 9: Interleaved — Native-Write Then Rust-Write
///
/// Native creates → attach → write region A (offset 0) → detach → close →
/// Rust opens → writes region B (second block) → flush → close →
/// native opens → attach → reads both regions → both intact.
#[pal_async::async_test]
async fn interleaved_native_then_rust(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 32 * 1024 * 1024; // native default

    // Step 1: Native create + write region A at offset 0.
    {
        let mut native = NativeVhdx::create_dynamic(&vhdx_path, 1024 * 1024 * 1024, 0, 0);
        let raw = native.attach_raw();
        let pattern_a = test_pattern(0, 512);
        let written = raw.write_at(0, &pattern_a).expect("native write region A");
        assert_eq!(written, 512);
    }

    // Step 2: Rust opens → writes region B at block_size offset.
    {
        let rust = RustVhdx::open(&vhdx_path, false, Some(&driver)).await;
        let pattern_b = test_pattern(block_size, 512);
        rust.write_data(block_size, &pattern_b).await;
        rust.flush().await;
        rust.close().await;
    }

    // Step 3: Native opens → reads both regions → verifies.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        let expected_a = test_pattern(0, 512);
        let mut buf_a = vec![0u8; 512];
        let bytes = raw.read_at(0, &mut buf_a).expect("read region A");
        assert_eq!(bytes, 512);
        assert_eq!(buf_a, expected_a, "region A corrupted");

        let expected_b = test_pattern(block_size, 512);
        let mut buf_b = vec![0u8; 512];
        let bytes = raw.read_at(block_size, &mut buf_b).expect("read region B");
        assert_eq!(bytes, 512);
        assert_eq!(buf_b, expected_b, "region B corrupted");
    }
}

/// Test 10: Interleaved — Rust-Write Then Native-Write
///
/// Rust creates → writes blocks 0, 2, 4 → flush → close →
/// native opens → attach → writes blocks 1, 3 → detach → close →
/// Rust opens → reads all blocks → all data intact.
#[pal_async::async_test]
async fn interleaved_rust_then_native(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024; // 2 MiB

    // Step 1: Rust create + write blocks 0, 2, 4.
    let rust_offsets = [0u64, 2 * block_size, 4 * block_size];
    {
        let rust = RustVhdx::create(&vhdx_path, 32 * 1024 * 1024, block_size as u32, &driver).await;
        for &off in &rust_offsets {
            rust.write_data(off, &test_pattern(off, 512)).await;
        }
        rust.flush().await;
        rust.close().await;
    }

    // Step 2: Native opens → writes blocks 1, 3.
    let native_offsets = [block_size, 3 * block_size];
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();
        for &off in &native_offsets {
            let pattern = test_pattern(off, 512);
            let written = raw.write_at(off, &pattern).expect("native write");
            assert_eq!(written, 512);
        }
    }

    // Step 3: Rust opens → reads all blocks → verifies.
    {
        let rust = RustVhdx::open(&vhdx_path, true, None).await;

        for &off in rust_offsets.iter().chain(native_offsets.iter()) {
            let expected = test_pattern(off, 512);
            let actual = rust.read_data(off, 512).await;
            assert_eq!(actual, expected, "data mismatch at offset {off:#x}");
        }

        rust.close().await;
    }
}

/// Test 11: Three-Way Round-Trip
///
/// Rust creates → writes block 0 → flush → close → native opens → attach →
/// writes block 1 → detach → close → Rust opens → reads blocks 0 and 1 →
/// both correct.
#[pal_async::async_test]
async fn three_way_round_trip(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Step 1: Rust creates and writes block 0.
    {
        let rust = RustVhdx::create(&vhdx_path, 16 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.flush().await;
        rust.close().await;
    }

    // Step 2: Native opens and writes block 1.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();
        let pattern = test_pattern(block_size, 512);
        let written = raw
            .write_at(block_size, &pattern)
            .expect("native write block 1");
        assert_eq!(written, 512);
    }

    // Step 3: Rust opens → reads blocks 0 and 1 → verifies.
    {
        let rust = RustVhdx::open(&vhdx_path, true, None).await;

        let data0 = rust.read_data(0, 512).await;
        assert_eq!(data0, test_pattern(0, 512), "block 0 data mismatch");

        let data1 = rust.read_data(block_size, 512).await;
        assert_eq!(
            data1,
            test_pattern(block_size, 512),
            "block 1 data mismatch"
        );

        rust.close().await;
    }
}

/// Test 12: Trim — Rust-Trim → Native-Read
///
/// Rust creates small disk (4 MiB, 2 MiB blocks) → writes all blocks →
/// trims block 1 → flush → close → native opens → attach →
/// raw-read block 0 (data intact) → raw-read block 1 (zeros).
#[pal_async::async_test]
async fn trim_rust_trim_native_read(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Rust create + write both blocks + trim block 1.
    {
        let rust = RustVhdx::create(&vhdx_path, 4 * 1024 * 1024, block_size as u32, &driver).await;

        // Write block 0 and block 1.
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.write_data(block_size, &test_pattern(block_size, 512))
            .await;
        rust.flush().await;

        // Trim block 1 entirely.
        rust.trim_range(block_size, block_size).await;
        rust.flush().await;
        rust.close().await;
    }

    // Native open + attach + verify.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    // Block 0 should still have data.
    let mut buf0 = vec![0u8; 512];
    let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
    assert_eq!(bytes, 512);
    assert_eq!(buf0, test_pattern(0, 512), "block 0 should be intact");

    // Block 1 should be zeros after trim.
    let mut buf1 = vec![0u8; 512];
    let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
    assert_eq!(bytes, 512);
    assert!(
        buf1.iter().all(|&b| b == 0),
        "block 1 should be zeros after trim"
    );
}

/// Test 13: Trim — Native-Write → Rust-Trim → Native-Read
///
/// Native creates → attach → writes blocks 0 and 1 → detach → close →
/// Rust opens → trims block 1 → flush → close → native opens → attach →
/// raw-read block 0 (intact) → raw-read block 1 (zeros).
#[pal_async::async_test]
async fn trim_native_write_rust_trim_native_read(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    // Native default block size is 32 MiB. Use a smaller Rust-created disk
    // so trim covers a full block efficiently.
    let block_size: u64 = 2 * 1024 * 1024;

    // Step 1: Rust creates to control block size, then close.
    {
        let rust = RustVhdx::create(&vhdx_path, 8 * 1024 * 1024, block_size as u32, &driver).await;
        rust.close().await;
    }

    // Step 2: Native writes blocks 0 and 1.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        let written = raw
            .write_at(0, &test_pattern(0, 512))
            .expect("native write block 0");
        assert_eq!(written, 512);

        let written = raw
            .write_at(block_size, &test_pattern(block_size, 512))
            .expect("native write block 1");
        assert_eq!(written, 512);
    }

    // Step 3: Rust opens → trims block 1 → flush → close.
    {
        let rust = RustVhdx::open(&vhdx_path, false, Some(&driver)).await;
        rust.trim_range(block_size, block_size).await;
        rust.flush().await;
        rust.close().await;
    }

    // Step 4: Native opens → reads → verifies.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        // Block 0 intact.
        let mut buf0 = vec![0u8; 512];
        let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
        assert_eq!(bytes, 512);
        assert_eq!(buf0, test_pattern(0, 512), "block 0 should be intact");

        // Block 1 zeros.
        let mut buf1 = vec![0u8; 512];
        let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
        assert_eq!(bytes, 512);
        assert!(
            buf1.iter().all(|&b| b == 0),
            "block 1 should be zeros after trim"
        );
    }
}

// =====================================================================
// Differencing Disk Helpers
// =====================================================================

impl NativeVhdx {
    /// Create a differencing VHDX child (parent must already exist).
    fn create_differencing(path: &Path, parent_path: &Path) -> Self {
        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };

        let wide = to_wide(path);
        let parent_wide = to_wide(parent_path);

        let mut params = CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: CREATE_VIRTUAL_DISK_VERSION_2,
            ..Default::default()
        };
        // ParentPath tells CreateVirtualDisk to create a differencing child.
        // MaximumSize, BlockSizeInBytes, and SectorSizeInBytes are inherited
        // from the parent (set to 0 / left default).
        params.Anonymous.Version2.ParentPath = PCWSTR(parent_wide.as_ptr());

        let mut handle = HANDLE::default();

        // SAFETY: All parameters are correctly initialized, paths are
        // null-terminated, and handle is written by the API on success.
        // `parent_wide` is alive for the duration of this call.
        let result = unsafe {
            CreateVirtualDisk(
                &storage_type,
                PCWSTR(wide.as_ptr()),
                VIRTUAL_DISK_ACCESS_MASK(0),
                None,
                CREATE_VIRTUAL_DISK_FLAG_NONE,
                0,
                &params,
                None,
                &mut handle,
            )
        };
        assert!(
            result.is_ok(),
            "CreateVirtualDisk (differencing) failed: {result:?}"
        );

        NativeVhdx {
            handle,
            attached: false,
        }
    }
}

impl RustVhdx {
    /// Create a differencing VHDX via the Rust API (`has_parent: true`).
    ///
    /// No parent locator is written — this is sufficient for Rust-only
    /// chained reads but NOT for native-open.
    async fn create_diff(
        path: &Path,
        disk_size: u64,
        block_size: u32,
        driver: &DefaultDriver,
    ) -> Self {
        let file = StdFile::create(path).expect("create backing file");
        let mut params = vhdx::CreateParams {
            disk_size,
            block_size,
            has_parent: true,
            ..Default::default()
        };
        vhdx::create(&file, &mut params)
            .await
            .expect("vhdx create diff");
        drop(file);

        Self::open(path, false, Some(driver)).await
    }
}

/// Read data from a child, resolving Unmapped ranges from the parent.
///
/// For each `ReadRange::Unmapped` in the child's read resolution,
/// reads the corresponding range from the parent. `Data` and `Zero`
/// ranges are handled normally from the child.
async fn chained_read(child: &RustVhdx, parent: &RustVhdx, offset: u64, len: u32) -> Vec<u8> {
    let mut ranges = Vec::new();
    let guard = child
        .vhdx
        .resolve_read(offset, len, &mut ranges)
        .await
        .expect("child resolve_read");

    let mut result = vec![0u8; len as usize];

    for range in &ranges {
        match range {
            ReadRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let buf_offset = (*guest_offset - offset) as usize;
                let buf_len = *length as usize;
                child
                    .io_file
                    .read_at(*file_offset, &mut result[buf_offset..buf_offset + buf_len])
                    .await
                    .expect("read child data");
            }
            ReadRange::Zero {
                guest_offset,
                length,
            } => {
                // Already zero-initialized in result.
                let _ = (guest_offset, length);
            }
            ReadRange::Unmapped {
                guest_offset,
                length,
            } => {
                // Fall through to parent.
                let parent_data = parent.read_data(*guest_offset, *length).await;
                let buf_offset = (*guest_offset - offset) as usize;
                result[buf_offset..buf_offset + parent_data.len()].copy_from_slice(&parent_data);
            }
        }
    }

    drop(guard);
    result
}

// =====================================================================
// Differencing Disk Test Cases
// =====================================================================

/// Test 14: Rust-Only Chained Read — Unwritten Child
///
/// Rust creates parent + writes data → Rust creates diff child →
/// child read returns zeros (Unmapped) → chained_read falls through
/// to parent → data matches.
#[pal_async::async_test]
async fn diff_rust_chained_read_unwritten_child(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    let block_size: u32 = 2 * 1024 * 1024;
    let disk_size: u64 = 4 * 1024 * 1024;

    // Step 1: Rust-create parent, write test_pattern at offset 0.
    {
        let parent = RustVhdx::create(&parent_path, disk_size, block_size, &driver).await;
        parent.write_data(0, &test_pattern(0, 512)).await;
        parent.flush().await;
        parent.close().await;
    }

    // Step 2: Rust-create diff child (has_parent: true).
    let child = RustVhdx::create_diff(&child_path, disk_size, block_size, &driver).await;

    // Step 3: child.read_data returns zeros (Unmapped treated as zero).
    let child_data = child.read_data(0, 512).await;
    assert!(
        child_data.iter().all(|&b| b == 0),
        "unwritten child should return zeros"
    );

    // Step 4: chained_read falls through to parent.
    let parent = RustVhdx::open(&parent_path, true, None).await;
    let chained = chained_read(&child, &parent, 0, 512).await;
    assert_eq!(
        chained,
        test_pattern(0, 512),
        "chained read should return parent data"
    );

    // Step 5: Verify child is a differencing disk.
    assert!(child.vhdx.has_parent(), "child should have has_parent set");

    child.close().await;
    parent.close().await;
}

/// Test 15: Rust-Only Chained Read — Partial Block Write
///
/// Rust creates parent + writes 2 sectors → Rust creates diff child →
/// writes 1 sector to child with different data → chained_read returns
/// child data for written sector, parent data for unwritten sector.
///
/// This exercises PartiallyPresent block handling: the Rust write allocates
/// the block as PartiallyPresent (not FullyPresent), and the sector bitmap
/// tracks which sectors are present in the child vs. transparent to parent.
#[pal_async::async_test]
async fn diff_rust_chained_read_partial_block(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    let block_size: u32 = 2 * 1024 * 1024;
    let disk_size: u64 = 4 * 1024 * 1024;

    // Step 1: Rust-create parent, write 2 sectors at offset 0.
    {
        let parent = RustVhdx::create(&parent_path, disk_size, block_size, &driver).await;
        parent.write_data(0, &test_pattern(0, 1024)).await;
        parent.flush().await;
        parent.close().await;
    }

    // Step 2: Rust-create diff child.
    let child = RustVhdx::create_diff(&child_path, disk_size, block_size, &driver).await;

    // Step 3: Write only sector 0 in child with a distinguishable pattern.
    // The block should become PartiallyPresent with SBM bit 0 set.
    let child_pattern = vec![0xAA; 512];
    child.write_data(0, &child_pattern).await;
    child.flush().await;

    // Step 4: chained_read should return child data for sector 0,
    //         parent data for sector 1.
    let parent = RustVhdx::open(&parent_path, true, None).await;
    let chained = chained_read(&child, &parent, 0, 1024).await;

    // Sector 0 (bytes 0..512): from child → [0xAA; 512]
    assert_eq!(
        &chained[..512],
        &child_pattern[..],
        "sector 0 should come from child"
    );
    // Sector 1 (bytes 512..1024): from parent → test_pattern(512, 512)
    assert_eq!(
        &chained[512..1024],
        &test_pattern(512, 512)[..],
        "sector 1 should come from parent"
    );

    child.close().await;
    parent.close().await;
}

/// Test 16: Native-Create Diff → Rust Reads
///
/// Native creates parent + writes data → native creates diff child →
/// writes different data to child block 0 → Rust opens child → reads
/// child data for block 0 + Unmapped for block 1 → chained_read resolves
/// parent data for block 1.
#[pal_async::async_test]
async fn diff_native_create_rust_reads() {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    // Native default: 32 MiB blocks.
    let block_size: u64 = 32 * 1024 * 1024;

    // Step 1: Native-create parent (1 GiB).
    // Write test_pattern at offset 0 and offset block_size.
    {
        let mut native = NativeVhdx::create_dynamic(&parent_path, 1024 * 1024 * 1024, 0, 0);
        let raw = native.attach_raw();

        let written = raw
            .write_at(0, &test_pattern(0, 512))
            .expect("write parent block 0");
        assert_eq!(written, 512);

        let written = raw
            .write_at(block_size, &test_pattern(block_size, 512))
            .expect("write parent block 1");
        assert_eq!(written, 512);
    }

    // Step 2: Native-create differencing child.
    // Write [0xBB; 512] at offset 0 (overwrites parent's block 0).
    {
        let mut native = NativeVhdx::create_differencing(&child_path, &parent_path);
        let raw = native.attach_raw();

        let child_data = vec![0xBBu8; 512];
        let written = raw.write_at(0, &child_data).expect("write child block 0");
        assert_eq!(written, 512);
    }

    // Step 3: Rust opens child (read-only).
    let child = RustVhdx::open(&child_path, true, None).await;

    // Block 0, sector 0: child has data → should be [0xBB; 512].
    let data_block0 = child.read_data(0, 512).await;
    assert_eq!(
        data_block0,
        vec![0xBBu8; 512],
        "child block 0 sector 0 should be 0xBB"
    );

    // Block 1: Unmapped in child → read_data returns zeros.
    let data_block1 = child.read_data(block_size, 512).await;
    assert!(
        data_block1.iter().all(|&b| b == 0),
        "child block 1 should be zeros (Unmapped)"
    );

    // Step 4: Rust opens parent (read-only).
    let parent = RustVhdx::open(&parent_path, true, None).await;

    // Verify parent block 1 data directly.
    let parent_block1 = parent.read_data(block_size, 512).await;
    assert_eq!(
        parent_block1,
        test_pattern(block_size, 512),
        "parent block 1 should have original data"
    );

    // Step 5: chained_read for block 1 → falls through to parent.
    let chained = chained_read(&child, &parent, block_size, 512).await;
    assert_eq!(
        chained,
        test_pattern(block_size, 512),
        "chained read block 1 should return parent data"
    );

    child.close().await;
    parent.close().await;
}

/// Test 17: Native-Create Diff → Rust Reads Empty Child
///
/// Native creates parent + writes data → native creates diff child →
/// no writes to child → Rust reads child → all Unmapped → chained read
/// falls through to parent.
#[pal_async::async_test]
async fn diff_native_create_empty_child_rust_reads() {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    // Step 1: Native-create parent (1 GiB), write data at offset 0.
    {
        let mut native = NativeVhdx::create_dynamic(&parent_path, 1024 * 1024 * 1024, 0, 0);
        let raw = native.attach_raw();

        let written = raw
            .write_at(0, &test_pattern(0, 512))
            .expect("write parent");
        assert_eq!(written, 512);
    }

    // Step 2: Native-create differencing child (no writes).
    {
        let _native = NativeVhdx::create_differencing(&child_path, &parent_path);
    }

    // Step 3: Rust opens child.
    let child = RustVhdx::open(&child_path, true, None).await;

    // Child has_parent should be true.
    assert!(child.vhdx.has_parent(), "child should be a diff disk");

    // read_data returns zeros (Unmapped).
    let child_data = child.read_data(0, 512).await;
    assert!(
        child_data.iter().all(|&b| b == 0),
        "empty child should return zeros"
    );

    // Step 4: Rust opens parent; chained_read falls through.
    let parent = RustVhdx::open(&parent_path, true, None).await;
    let chained = chained_read(&child, &parent, 0, 512).await;
    assert_eq!(
        chained,
        test_pattern(0, 512),
        "chained read should return parent data"
    );

    child.close().await;
    parent.close().await;
}

/// Test 18: Rust Writes to Native-Created Diff
///
/// Native creates parent → writes data at offsets 0 and 512 → native creates
/// diff child → Rust opens child writable → writes sector 0 with different
/// data → close → native opens child (with parent chain) → attach →
/// raw-read → child data present at sector 0, parent data for sector 1
/// (unwritten in child, falls through via native chain and SBM resolution).
#[pal_async::async_test]
async fn diff_rust_writes_to_native_diff(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    // Step 1: Native-create parent (1 GiB), write data at offsets 0 and 512.
    {
        let mut native = NativeVhdx::create_dynamic(&parent_path, 1024 * 1024 * 1024, 0, 0);
        let raw = native.attach_raw();

        let written = raw
            .write_at(0, &test_pattern(0, 512))
            .expect("write parent sector 0");
        assert_eq!(written, 512);

        let written = raw
            .write_at(512, &test_pattern(512, 512))
            .expect("write parent sector 1");
        assert_eq!(written, 512);
    }

    // Step 2: Native-create diff child (no writes yet).
    {
        let _native = NativeVhdx::create_differencing(&child_path, &parent_path);
    }

    // Step 3: Rust opens child writable, writes only sector 0.
    // The block should become PartiallyPresent with SBM bit 0 set.
    {
        let child = RustVhdx::open(&child_path, false, Some(&driver)).await;
        let child_data = vec![0xCCu8; 512];
        child.write_data(0, &child_data).await;
        child.flush().await;
        child.close().await;
    }

    // Step 4: Native opens child (chain resolves automatically).
    // Sector 0: from child (SBM bit set) → [0xCC; 512]
    // Sector 1: from parent (SBM bit clear, falls through) → test_pattern(512, 512)
    {
        let mut native = NativeVhdx::open(&child_path, false);
        let raw = native.attach_raw();

        let mut buf0 = vec![0u8; 512];
        let bytes = raw.read_at(0, &mut buf0).expect("read child sector 0");
        assert_eq!(bytes, 512);
        assert_eq!(buf0, vec![0xCCu8; 512], "sector 0 should be child's data");

        let mut buf1 = vec![0u8; 512];
        let bytes = raw.read_at(512, &mut buf1).expect("read child sector 1");
        assert_eq!(bytes, 512);
        assert_eq!(
            buf1,
            test_pattern(512, 512),
            "sector 1 should come from parent via chain"
        );
    }
}

/// Test 19: Rust Writes + Trims in Diff Child
///
/// Rust-create parent → write data to blocks 0 and 1 → native-create diff
/// child → Rust writes to child blocks 0 and 1 → Rust trims block 1 →
/// native reads → block 0 has child data, block 1 is zeros.
#[pal_async::async_test]
async fn diff_rust_writes_and_trims(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.vhdx");
    let child_path = dir.path().join("child.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Step 1: Rust-create parent (to control block size), write blocks 0 and 1.
    {
        let parent =
            RustVhdx::create(&parent_path, 8 * 1024 * 1024, block_size as u32, &driver).await;
        parent.write_data(0, &test_pattern(0, 512)).await;
        parent
            .write_data(block_size, &test_pattern(block_size, 512))
            .await;
        parent.flush().await;
        parent.close().await;
    }

    // Step 2: Native-create diff child.
    {
        let _native = NativeVhdx::create_differencing(&child_path, &parent_path);
    }

    // Step 3: Rust opens child writable.
    //   - Write [0xDD; 512] at offset 0 (block 0, sector 0)
    //   - Write [0xEE; 512] at offset block_size (block 1, sector 0)
    //   - Trim block 1 entirely
    {
        let child = RustVhdx::open(&child_path, false, Some(&driver)).await;
        child.write_data(0, &vec![0xDDu8; 512]).await;
        child.write_data(block_size, &vec![0xEEu8; 512]).await;
        child.trim_range(block_size, block_size).await;
        child.flush().await;
        child.close().await;
    }

    // Step 4: Native opens child (chain). Attach + read.
    {
        let mut native = NativeVhdx::open(&child_path, false);
        let raw = native.attach_raw();

        // Block 0: child's write → [0xDD; 512]
        let mut buf0 = vec![0u8; 512];
        let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
        assert_eq!(bytes, 512);
        assert_eq!(buf0, vec![0xDDu8; 512], "block 0 should be child's data");

        // Block 1: trimmed → zeros (TrimMode::Zero makes block Zero state;
        // through native chain, Zero means zeros).
        let mut buf1 = vec![0u8; 512];
        let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
        assert_eq!(bytes, 512);
        assert!(
            buf1.iter().all(|&b| b == 0),
            "block 1 should be zeros after trim"
        );
    }
}

// =====================================================================
// Log Replay Cross-Validation
// =====================================================================
//
// These tests exercise crash recovery scenarios where the Rust stack
// writes data with a dirty log (via `abort()`), and the native Windows
// VHD stack replays the log on open — or vice versa.
//
// The key API for simulating a crash in the Rust stack is `VhdxFile::abort()`:
// it drops the log channel without flushing, leaving the log GUID set in
// the header. The next open (by either stack) must replay the log before
// the file is usable.
//
// The native Windows VHD stack always performs a clean close on handle drop
// (it flushes the log and clears the log GUID), so we cannot easily create
// a dirty log via native. Tests focus on Rust-crash → Native-replay and
// full lifecycle interleaving scenarios.

/// Test 20: Rust Crash → Native Replay
///
/// Rust opens writable → writes data to two blocks → flush → abort
/// (simulated crash, log stays dirty) → native opens (replays log) →
/// attach → raw-read → data is present and correct.
#[pal_async::async_test]
async fn log_replay_rust_crash_native_reads(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Rust create + write + flush + abort (crash).
    {
        let rust = RustVhdx::create(&vhdx_path, 8 * 1024 * 1024, block_size as u32, &driver).await;

        // Write to blocks 0 and 1.
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.write_data(block_size, &test_pattern(block_size, 512))
            .await;
        rust.flush().await;

        // Abort — leaves dirty log (log_guid is set in header).
        rust.abort().await;
    }

    // Native opens — should replay the dirty log automatically.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    // Block 0: should have Rust's data after log replay.
    let mut buf0 = vec![0u8; 512];
    let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
    assert_eq!(bytes, 512);
    assert_eq!(buf0, test_pattern(0, 512), "block 0 data after log replay");

    // Block 1: should have Rust's data after log replay.
    let mut buf1 = vec![0u8; 512];
    let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
    assert_eq!(bytes, 512);
    assert_eq!(
        buf1,
        test_pattern(block_size, 512),
        "block 1 data after log replay"
    );
}

/// Test 21: Rust Crash (Multiple Blocks) → Native Replay
///
/// Rust opens writable → writes data to many blocks across the disk
/// (enough to exercise multiple log entries / batch commits) → flush →
/// abort → native opens (replays all log entries) → all data intact.
#[pal_async::async_test]
async fn log_replay_rust_crash_many_blocks_native_reads(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;
    let block_count = 8u64;
    let disk_size = block_size * (block_count + 1);

    // Rust create + write all blocks + flush + abort.
    {
        let rust = RustVhdx::create(&vhdx_path, disk_size, block_size as u32, &driver).await;

        for i in 0..block_count {
            let offset = i * block_size;
            rust.write_data(offset, &test_pattern(offset, 512)).await;
        }
        rust.flush().await;
        rust.abort().await;
    }

    // Native opens (replays log) → attach → read all blocks.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    for i in 0..block_count {
        let offset = i * block_size;
        let expected = test_pattern(offset, 512);
        let mut buf = vec![0u8; 512];
        let bytes = raw.read_at(offset, &mut buf).expect("native read");
        assert_eq!(bytes, 512);
        assert_eq!(
            buf, expected,
            "data mismatch at block {i} (offset {offset:#x})"
        );
    }
}

/// Test 22: Rust Crash → Rust Replay → Native Reads
///
/// Rust writes → flush → abort → Rust reopens writable (replays log) →
/// clean close → native opens → data intact. This verifies Rust's own
/// log replay produces a file the native stack accepts.
#[pal_async::async_test]
async fn log_replay_rust_crash_rust_replay_native_reads(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Rust create + write + flush + abort.
    {
        let rust = RustVhdx::create(&vhdx_path, 8 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.write_data(block_size, &test_pattern(block_size, 512))
            .await;
        rust.flush().await;
        rust.abort().await;
    }

    // Rust reopens writable (replays log) → verify data → clean close.
    {
        let rust = RustVhdx::open(&vhdx_path, false, Some(&driver)).await;
        let data0 = rust.read_data(0, 512).await;
        assert_eq!(data0, test_pattern(0, 512), "block 0 after Rust replay");
        let data1 = rust.read_data(block_size, 512).await;
        assert_eq!(
            data1,
            test_pattern(block_size, 512),
            "block 1 after Rust replay"
        );
        rust.close().await;
    }

    // Native opens the cleanly-closed file → data intact.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    let mut buf0 = vec![0u8; 512];
    let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
    assert_eq!(bytes, 512);
    assert_eq!(buf0, test_pattern(0, 512), "block 0 via native");

    let mut buf1 = vec![0u8; 512];
    let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
    assert_eq!(bytes, 512);
    assert_eq!(buf1, test_pattern(block_size, 512), "block 1 via native");
}

/// Test 23: Rust Crash → Native Replay → Native Writes More → Rust Reads
///
/// Full lifecycle: Rust writes block 0 → abort (crash) → native opens
/// (replays log) → native writes block 1 → close → Rust opens → reads
/// both blocks → both correct.
#[pal_async::async_test]
async fn log_replay_lifecycle_crash_replay_more_writes(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Step 1: Rust create + write block 0 + flush + abort.
    {
        let rust = RustVhdx::create(&vhdx_path, 16 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.flush().await;
        rust.abort().await;
    }

    // Step 2: Native opens (replays dirty log) → writes block 1 → closes.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        // Verify block 0 survived replay.
        let mut buf = vec![0u8; 512];
        let bytes = raw.read_at(0, &mut buf).expect("read block 0 after replay");
        assert_eq!(bytes, 512);
        assert_eq!(buf, test_pattern(0, 512), "block 0 after native replay");

        // Write block 1.
        let pattern = test_pattern(block_size, 512);
        let written = raw
            .write_at(block_size, &pattern)
            .expect("native write block 1");
        assert_eq!(written, 512);
    }

    // Step 3: Rust opens → reads both blocks → verifies.
    {
        let rust = RustVhdx::open(&vhdx_path, true, None).await;

        let data0 = rust.read_data(0, 512).await;
        assert_eq!(data0, test_pattern(0, 512), "block 0 via Rust");

        let data1 = rust.read_data(block_size, 512).await;
        assert_eq!(data1, test_pattern(block_size, 512), "block 1 via Rust");

        rust.close().await;
    }
}

/// Test 24: Rust Crash With Trim → Native Replay
///
/// Rust creates → writes blocks 0 and 1 → trims block 1 → flush → abort →
/// native opens (replays log) → block 0 intact, block 1 is zeros.
/// Verifies that trim state is correctly captured in the WAL and replayed.
#[pal_async::async_test]
async fn log_replay_rust_crash_with_trim_native_reads(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Rust create + write both blocks + trim block 1 + flush + abort.
    {
        let rust = RustVhdx::create(&vhdx_path, 8 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.write_data(block_size, &test_pattern(block_size, 512))
            .await;
        rust.flush().await;

        // Trim block 1 → BAT state change (Zero or Unmapped).
        rust.trim_range(block_size, block_size).await;
        rust.flush().await;

        rust.abort().await;
    }

    // Native opens (replays log including the trim BAT update).
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    // Block 0: should have data.
    let mut buf0 = vec![0u8; 512];
    let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
    assert_eq!(bytes, 512);
    assert_eq!(buf0, test_pattern(0, 512), "block 0 should be intact");

    // Block 1: should be zeros (trimmed).
    let mut buf1 = vec![0u8; 512];
    let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
    assert_eq!(bytes, 512);
    assert!(
        buf1.iter().all(|&b| b == 0),
        "block 1 zeros after trim + crash + log replay"
    );
}

/// Test 25: Multiple Crash-Recovery Cycles via Native
///
/// Rust writes → crash → native opens (replays) → writes more → close →
/// Rust writes → crash → native opens (replays) → all data intact.
/// Verifies that the log replay leaves the file in a clean state that
/// supports another full write-crash-recovery cycle.
#[pal_async::async_test]
async fn log_replay_repeated_crash_cycles(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Cycle 1: Rust writes block 0 → crash.
    {
        let rust = RustVhdx::create(&vhdx_path, 16 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.flush().await;
        rust.abort().await;
    }

    // Cycle 1 recovery: Native opens (replays) → writes block 1 → closes.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        // Verify block 0 survived.
        let mut buf = vec![0u8; 512];
        raw.read_at(0, &mut buf).expect("read block 0");
        assert_eq!(buf, test_pattern(0, 512), "cycle 1: block 0");

        // Write block 1.
        let written = raw
            .write_at(block_size, &test_pattern(block_size, 512))
            .expect("native write block 1");
        assert_eq!(written, 512);
    }

    // Cycle 2: Rust opens (clean file now) → writes block 2 → crash.
    {
        let rust = RustVhdx::open(&vhdx_path, false, Some(&driver)).await;
        rust.write_data(2 * block_size, &test_pattern(2 * block_size, 512))
            .await;
        rust.flush().await;
        rust.abort().await;
    }

    // Cycle 2 recovery: Native opens (replays) → reads all 3 blocks.
    {
        let mut native = NativeVhdx::open(&vhdx_path, false);
        let raw = native.attach_raw();

        let mut buf0 = vec![0u8; 512];
        raw.read_at(0, &mut buf0).expect("read block 0");
        assert_eq!(buf0, test_pattern(0, 512), "cycle 2: block 0");

        let mut buf1 = vec![0u8; 512];
        raw.read_at(block_size, &mut buf1).expect("read block 1");
        assert_eq!(buf1, test_pattern(block_size, 512), "cycle 2: block 1");

        let mut buf2 = vec![0u8; 512];
        raw.read_at(2 * block_size, &mut buf2)
            .expect("read block 2");
        assert_eq!(buf2, test_pattern(2 * block_size, 512), "cycle 2: block 2");
    }
}

/// Test 26: Clean Rust File → Native Opens Without Replay
///
/// Rust creates → writes → flush → close (clean shutdown) → native opens →
/// data intact. A cleanly-closed file should not trigger log replay.
#[pal_async::async_test]
async fn log_replay_clean_close_no_replay_needed(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let vhdx_path = dir.path().join("test.vhdx");

    let block_size: u64 = 2 * 1024 * 1024;

    // Rust create + write + flush + clean close.
    {
        let rust = RustVhdx::create(&vhdx_path, 8 * 1024 * 1024, block_size as u32, &driver).await;
        rust.write_data(0, &test_pattern(0, 512)).await;
        rust.write_data(block_size, &test_pattern(block_size, 512))
            .await;
        rust.flush().await;
        rust.close().await;
    }

    // Native opens — should succeed without needing log replay.
    let mut native = NativeVhdx::open(&vhdx_path, false);
    let raw = native.attach_raw();

    let mut buf0 = vec![0u8; 512];
    let bytes = raw.read_at(0, &mut buf0).expect("read block 0");
    assert_eq!(bytes, 512);
    assert_eq!(buf0, test_pattern(0, 512), "block 0");

    let mut buf1 = vec![0u8; 512];
    let bytes = raw.read_at(block_size, &mut buf1).expect("read block 1");
    assert_eq!(bytes, 512);
    assert_eq!(buf1, test_pattern(block_size, 512), "block 1");
}
