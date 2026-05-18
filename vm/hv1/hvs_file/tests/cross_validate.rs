// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-validation test: writes a .vmrs file with hvs_file, then opens it
//! with VmSavedStateDumpProvider.dll from the Windows SDK and verifies that
//! the DLL can parse VP count and architecture.
//!
//! Skips gracefully if the DLL is not found on the system.

#![cfg(windows)]
#![allow(unsafe_code)]

use hvdef::save_restore::*;
use hvs_file::reader::ValueType;
use hvs_file::writer::HvsFileWriter;
use std::ffi::c_void;
use std::io::Cursor;
use std::mem::size_of;
use std::path::PathBuf;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Read a key from `reader` and write it to `writer`, preserving its type.
fn copy_key<R: std::io::Read + std::io::Seek, W: std::io::Write + std::io::Seek>(
    reader: &mut hvs_file::reader::HvsFileReader<R>,
    writer: &mut HvsFileWriter<W>,
    key: &str,
) {
    match reader.value_type(key).unwrap() {
        ValueType::Int => writer.add_int(key, reader.read_int(key).unwrap()),
        ValueType::UInt => writer.add_uint(key, reader.read_uint(key).unwrap()),
        ValueType::String => writer.add_string(key, &reader.read_string(key).unwrap()),
        ValueType::Bool => writer.add_bool(key, reader.read_bool(key).unwrap()),
        ValueType::Array => writer.add_array(key, reader.read_array(key).unwrap()).unwrap(),
    }
}

mod dll {
    use std::ffi::c_void;

    pal::delayload! { "vmsavedstatedumpprovider.dll" {
        pub fn LoadSavedStateFile(
            vmrs_file: *const u16,
            handle: *mut *mut c_void
        ) -> i32;

        pub fn ReleaseSavedStateFiles(
            handle: *mut c_void
        ) -> i32;

        pub fn GetVpCount(
            handle: *mut c_void,
            vp_count: *mut u32
        ) -> i32;

        pub fn GetArchitecture(
            handle: *mut c_void,
            vp_id: u32,
            arch: *mut u32
        ) -> i32;
    }}
}

/// Try to find VmSavedStateDumpProvider.dll in the Windows SDK and add
/// its directory to the DLL search path.
fn setup_dll_search_path() -> bool {
    let kits_root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if !kits_root.exists() {
        return false;
    }

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        return false;
    };

    // Find SDK version directories sorted descending (latest first)
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&kits_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    versions.sort();

    for version_dir in versions.iter().rev() {
        let dll_dir = version_dir.join(arch);
        let dll_path = dll_dir.join("vmsavedstatedumpprovider.dll");
        if dll_path.exists() {
            // Add the directory to the DLL search path via SetDllDirectoryW
            let wide_dir: Vec<u16> = dll_dir
                .to_str()
                .unwrap()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn SetDllDirectoryW(path: *const u16) -> i32;
                }
                SetDllDirectoryW(wide_dir.as_ptr());
            }
            return true;
        }
    }
    false
}

/// Build a minimal partition state blob that VmSavedStateDumpProvider can parse.
fn build_partition_state_blob(rip: u64, cr3: u64) -> Vec<u8> {
    let mut blob = Vec::new();
    let header_size = size_of::<VmSaveChunkHeader>();

    // VID_SAVED_STATE_DESCRIPTOR
    let descriptor = VidSavedStateDescriptor {
        descriptor_size: size_of::<VidSavedStateDescriptor>() as u64,
        header_size: size_of::<VidSavedStateDescriptor>() as u64,
        total_size: 0, // filled in later
    };
    blob.extend_from_slice(descriptor.as_bytes());

    // 16-byte alignment padding after descriptor
    blob.extend_from_slice(&[0u8; 16]);

    // Prolog
    let mut prolog = ObSaveChunkProlog::new_zeroed();
    prolog.header.id = VmSaveChunkId::PROLOG;
    prolog.header.data_length = (OB_SAVE_CHUNK_PROLOG_SIZE - header_size) as u32;
    prolog.undefined_tag = VM_SAVE_CHUNK_TAG_UNDEFINED;
    prolog.vendor = HvProcessorVendor::INTEL;
    blob.extend_from_slice(prolog.as_bytes());

    // OsId
    let mut osid = PtSaveChunkOsId::new_zeroed();
    osid.header.id = VmSaveChunkId::OS_ID;
    osid.header.data_length = (size_of::<PtSaveChunkOsId>() - header_size) as u32;
    blob.extend_from_slice(osid.as_bytes());

    // VpIndices — 1 VP (VP 0)
    let mut vp_indices = VpSaveChunkVpIndices::new_zeroed();
    vp_indices.header.id = VmSaveChunkId::VP_INDICES;
    vp_indices.header.data_length =
        (size_of::<VpSaveChunkVpIndices>() - header_size) as u32;
    vp_indices.bsp = 0;
    vp_indices.vp_present_map[0] = 0x01;
    blob.extend_from_slice(vp_indices.as_bytes());

    // Vp marker
    let mut vp = ObSaveChunkVp::new_zeroed();
    vp.header.id = VmSaveChunkId::VP;
    vp.header.data_length = (size_of::<ObSaveChunkVp>() - header_size) as u32;
    vp.vp_index = 0;
    blob.extend_from_slice(vp.as_bytes());

    // GP registers
    let mut gp = VpX64SaveChunkGpRegisters::new_zeroed();
    gp.header.id = VmSaveChunkId::VP_GP_REGISTERS;
    gp.header.data_length =
        (size_of::<VpX64SaveChunkGpRegisters>() - header_size) as u32;
    gp.rip = rip;
    gp.rsp = 0xFFFFF780_00000000;
    gp.rax = 0xDEAD_BEEF;
    blob.extend_from_slice(gp.as_bytes());

    // Control registers
    let mut cr = SynicX64SaveChunkControlRegisters::new_zeroed();
    cr.header.id = VmSaveChunkId::VP_VTL_CONTROL_REGISTERS;
    cr.header.data_length =
        (size_of::<SynicX64SaveChunkControlRegisters>() - header_size) as u32;
    cr.cr0 = 0x80050033;
    cr.cr3 = cr3;
    cr.cr4 = 0x370678;
    cr.efer = 0xD01;
    blob.extend_from_slice(cr.as_bytes());

    // Segment registers
    let mut seg = VpX64SaveChunkSegmentRegisters::new_zeroed();
    seg.header.id = VmSaveChunkId::VP_SEGMENT_REGISTERS;
    seg.header.data_length =
        (size_of::<VpX64SaveChunkSegmentRegisters>() - header_size) as u32;
    seg.cs.selector = 0x10;
    seg.cs.attributes = 0x209B;
    seg.cs.limit = 0xFFFFFFFF;
    blob.extend_from_slice(seg.as_bytes());

    // Table registers
    let mut table = VpX64SaveChunkTableRegisters::new_zeroed();
    table.header.id = VmSaveChunkId::VP_TABLE_REGISTERS;
    table.header.data_length =
        (size_of::<VpX64SaveChunkTableRegisters>() - header_size) as u32;
    table.idtr.limit = 0x0FFF;
    table.gdtr.limit = 0x006F;
    blob.extend_from_slice(table.as_bytes());

    // Epilog
    let mut epilog = ObSaveChunkEpilog::new_zeroed();
    epilog.header.id = VmSaveChunkId::EPILOG;
    epilog.header.data_length = 0;
    blob.extend_from_slice(epilog.as_bytes());

    // Fix up total size in the descriptor
    let total_size = blob.len() as u64;
    blob[16..24].copy_from_slice(&total_size.to_le_bytes());

    blob
}

/// Build a complete .vmrs file in memory with fully synthetic content.
fn build_vmrs_file(rip: u64, cr3: u64) -> Vec<u8> {
    let partition_state = build_partition_state_blob(rip, cr3);

    // MEMORY_BLOCK_OBJECT_SAVE_STRUCT_CURRENT (48 bytes):
    //   m_SavedStateVersion: u32    offset 0
    //   m_Flags:             u32    offset 4
    //   m_PageCountTotal:    u64    offset 8
    //   m_MbpIndexStart:     u64    offset 16
    //   m_GpaIndexStart:     u64    offset 24
    //   m_VirtualNode:       u32    offset 32
    //   (padding):           u32    offset 36
    //   m_KsrBlockId:        u64    offset 40
    let mut ram_meta = vec![0u8; 48];
    ram_meta[0..4].copy_from_slice(&3u32.to_le_bytes()); // m_SavedStateVersion = 3
    ram_meta[8..16].copy_from_slice(&1u64.to_le_bytes()); // m_PageCountTotal = 1

    let buf = Cursor::new(Vec::new());
    let mut w = HvsFileWriter::new(buf).unwrap();

    w.add_int("/savedstate/VmVersion", 0x0A00);
    w.add_int("/configuration/properties/version", 0x0A00);
    w.add_string("/savedstate/type", "Normal");
    w.add_string("/savedstate/VmwpVersion", "0.0.0.0");
    w.add_array("/savedstate/savedVM/partition_state", partition_state.to_vec())
        .unwrap();
    w.add_array("/savedstate/RamMemoryBlock0", ram_meta).unwrap();

    // One 4K page of zeros for RamBlock0
    let ram_data = vec![0u8; 4096];
    w.add_array("/savedstate/RamBlock0", ram_data)
        .unwrap();

    let buf = w.finish().unwrap();
    buf.into_inner()
}

/// Helper: write keys from the reference file, filtered by a predicate, and
/// try loading the result with the DLL. Returns the HRESULT.
fn try_with_keys(filter: impl Fn(&str) -> bool) -> i32 {
    let ref_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");

    let file = std::fs::File::open(&ref_path).unwrap();
    let mut reader = hvs_file::reader::HvsFileReader::open(file).unwrap();
    let all_keys: Vec<String> = reader.keys().map(|s| s.to_string()).collect();

    let buf = Cursor::new(Vec::new());
    let mut w = HvsFileWriter::new(buf).unwrap();

    for key in &all_keys {
        if !filter(key) {
            continue;
        }
        copy_key(&mut reader, &mut w, key);
    }

    let vmrs_data = w.finish().unwrap().into_inner();
    let vmrs_path = std::env::temp_dir().join("hvs_bisect_test.vmrs");
    std::fs::write(&vmrs_path, &vmrs_data).unwrap();
    eprintln!("  Wrote {} bytes to {}", vmrs_data.len(), vmrs_path.display());

    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let hr = unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);
        if hr >= 0 && !handle.is_null() {
            dll::ReleaseSavedStateFiles(handle);
        }
        hr
    };

    let _ = std::fs::remove_file(&vmrs_path);
    hr
}

/// Binary-search for the minimum set of keys the DLL needs.
#[test]
fn bisect_required_keys() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: DLL not loadable");
        return;
    }

    let ref_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");
    if !ref_path.exists() {
        eprintln!("SKIP: reference VMRS not found");
        return;
    }

    // All keys: should succeed
    let hr = try_with_keys(|_| true);
    eprintln!("All keys: 0x{:08X}", hr as u32);
    assert!(hr >= 0, "full round-trip failed");

    // Only /savedstate/*
    let hr = try_with_keys(|k| k.starts_with("/savedstate"));
    eprintln!("Only /savedstate/*: 0x{:08X}", hr as u32);

    // Only /savedstate/* minus /savedstate/configuration/*
    let hr = try_with_keys(|k| k.starts_with("/savedstate") && !k.starts_with("/savedstate/configuration"));
    eprintln!("savedstate minus config: 0x{:08X}", hr as u32);

    // Only the 4 essential keys + /savedstate/type + /savedstate/VmwpVersion
    let hr = try_with_keys(|k| matches!(k,
        "/savedstate/VmVersion" |
        "/savedstate/VmwpVersion" |
        "/savedstate/type" |
        "/savedstate/savedVM/partition_state" |
        "/savedstate/RamMemoryBlock0" |
        "/savedstate/RamBlock0"
    ));
    eprintln!("6 essential keys: 0x{:08X}", hr as u32);

    // Just the 4 we had before
    let hr = try_with_keys(|k| matches!(k,
        "/savedstate/VmVersion" |
        "/savedstate/savedVM/partition_state" |
        "/savedstate/RamMemoryBlock0" |
        "/savedstate/RamBlock0"
    ));
    eprintln!("4 original keys: 0x{:08X}", hr as u32);
}

#[test]
fn cross_validate_with_dll() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found, cannot locate VmSavedStateDumpProvider.dll");
        return;
    }

    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: VmSavedStateDumpProvider.dll not loadable");
        return;
    }

    let rip = 0xFFFFF802_12345678u64;
    let cr3 = 0x1AD000u64;

    let vmrs_data = build_vmrs_file(rip, cr3);

    // Write to a temp file
    let vmrs_path = std::env::temp_dir().join("hvs_file_cross_validate_test.vmrs");
    std::fs::write(&vmrs_path, &vmrs_data).unwrap();
    let _cleanup = defer(|| {
        let _ = std::fs::remove_file(&vmrs_path);
    });

    // Convert path to wide string
    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // Try loading the file with the DLL
    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);
        if hr < 0 {
            let debug_path = std::env::temp_dir().join("hvs_file_FAILED.vmrs");
            std::fs::copy(&vmrs_path, &debug_path).ok();
            panic!(
                "LoadSavedStateFile returned HRESULT 0x{hr:08X}\n\
                 File saved to {} for manual inspection.",
                debug_path.display()
            );
        }

        let _release = defer(|| {
            dll::ReleaseSavedStateFiles(handle);
        });

        // Verify VP count
        let mut vp_count: u32 = 0;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: HRESULT 0x{hr:08X}");
        assert_eq!(vp_count, 1, "expected 1 VP");

        // Verify architecture (Arch_x64 = 2 for our synthetic Intel blob)
        let mut arch: u32 = 0;
        let hr = dll::GetArchitecture(handle, 0, &mut arch);
        assert!(hr >= 0, "GetArchitecture failed: HRESULT 0x{hr:08X}");
        assert_eq!(arch, 2, "expected Arch_x64 (2)");

        eprintln!("Cross-validation PASSED: VP count = {vp_count}, arch = {arch}");
    }
}

/// Sanity check: load the reference config VMRS (which is NOT a saved state
/// file) and confirm the DLL returns `VM_SAVED_STATE_DUMP_E_PARTITION_STATE_NOT_FOUND`
/// (0xC0370500). This proves the Rust DLL binding works correctly — the DLL
/// successfully opens and parses the HVS file, then fails because there's no
/// partition state blob.
#[test]
fn dll_binding_sanity_check() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: DLL not loadable");
        return;
    }

    // The reference config VMRS checked into the repo
    let ref_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("88D07DD7-705E-4A4F-BDAF-3418782784ED.VMRS");

    if !ref_path.exists() {
        eprintln!("SKIP: reference VMRS not found at {}", ref_path.display());
        return;
    }

    let wide_path: Vec<u16> = ref_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);

        eprintln!("Reference config VMRS: HRESULT = 0x{:08X}", hr as u32);

        // Expected: 0xC0370500 = VM_SAVED_STATE_DUMP_E_PARTITION_STATE_NOT_FOUND
        // This means the DLL successfully opened and parsed the HVS file
        // structure, but didn't find the /savedstate/savedVM/partition_state key.
        assert_eq!(
            hr as u32, 0xC0370500,
            "Expected VM_SAVED_STATE_DUMP_E_PARTITION_STATE_NOT_FOUND, got 0x{:08X}",
            hr as u32
        );

        if hr >= 0 && !handle.is_null() {
            dll::ReleaseSavedStateFiles(handle);
        }
    }
}

/// Load the real saved state VMRS and verify the DLL parses it successfully.
#[test]
fn dll_loads_real_saved_state() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: DLL not loadable");
        return;
    }

    let ref_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");

    if !ref_path.exists() {
        eprintln!("SKIP: saved state VMRS not found at {}", ref_path.display());
        return;
    }

    let wide_path: Vec<u16> = ref_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);

        eprintln!("Saved state VMRS: HRESULT = 0x{:08X}", hr as u32);

        if hr < 0 {
            panic!("Failed to load real saved state: HRESULT 0x{:08X}", hr as u32);
        }

        let _release = defer(|| {
            dll::ReleaseSavedStateFiles(handle);
        });

        let mut vp_count: u32 = 0;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: 0x{:08X}", hr as u32);
        eprintln!("VP count: {vp_count}");

        let mut arch: u32 = 0;
        let hr = dll::GetArchitecture(handle, 0, &mut arch);
        assert!(hr >= 0, "GetArchitecture failed: 0x{:08X}", hr as u32);
        let arch_name = match arch { 1 => "x86", 2 => "x64", 3 => "ARM64", _ => "unknown" };
        eprintln!("Architecture: {arch_name} ({arch})");
    }
}

/// Simple scope guard for cleanup.
fn defer<F: FnOnce()>(f: F) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}

/// Verify the DLL can load a file that requires double object table chaining.
#[test]
fn object_table_chaining_with_dll() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: DLL not loadable");
        return;
    }

    // 226 usable slots per object table. 500 file objects + key tables
    // requires 3 chained object tables.
    let partition_state = build_partition_state_blob(0xFFFFF802_00000000, 0x1AD000);
    let mut ram_meta = vec![0u8; 48];
    ram_meta[0..4].copy_from_slice(&3u32.to_le_bytes());
    ram_meta[8..16].copy_from_slice(&500u64.to_le_bytes()); // 500 pages

    let buf = Cursor::new(Vec::new());
    let mut w = HvsFileWriter::new(buf).unwrap();

    w.add_int("/savedstate/VmVersion", 0x0A00);
    w.add_int("/configuration/properties/version", 0x0A00);
    w.add_array("/savedstate/savedVM/partition_state", partition_state).unwrap();
    w.add_array("/savedstate/RamMemoryBlock0", ram_meta).unwrap();

    // Write 500 RAM blocks with distinct data per block.
    for i in 0..500u32 {
        let mut block = vec![0u8; 4096];
        block[..4].copy_from_slice(&i.to_le_bytes());
        w.add_array(&format!("/savedstate/RamBlock{i}"), block).unwrap();
    }

    let vmrs_data = w.finish().unwrap().into_inner();
    let vmrs_path = std::env::temp_dir().join("hvs_chaining_test.vmrs");
    std::fs::write(&vmrs_path, &vmrs_data).unwrap();
    let _cleanup = defer(|| {
        let _ = std::fs::remove_file(&vmrs_path);
    });

    // Verify round-trip through our reader: spot-check a few blocks.
    {
        let file = std::fs::File::open(&vmrs_path).unwrap();
        let mut reader = hvs_file::reader::HvsFileReader::open(file).unwrap();
        for i in [0u32, 1, 249, 250, 499] {
            let data = reader.read_array(&format!("/savedstate/RamBlock{i}")).unwrap();
            assert_eq!(data.len(), 4096, "RamBlock{i} wrong size");
            let stamp = u32::from_le_bytes(data[..4].try_into().unwrap());
            assert_eq!(stamp, i, "RamBlock{i} data mismatch");
        }
    }

    // Verify the DLL can load and parse the file.
    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);
        if hr < 0 {
            panic!(
                "LoadSavedStateFile failed on double-chained object table file: HRESULT 0x{:08X}",
                hr as u32,
            );
        }
        let _release = defer(|| {
            dll::ReleaseSavedStateFiles(handle);
        });

        let mut vp_count: u32 = 0;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: 0x{:08X}", hr as u32);
        assert_eq!(vp_count, 1);

        let mut arch: u32 = 0;
        let hr = dll::GetArchitecture(handle, 0, &mut arch);
        assert!(hr >= 0, "GetArchitecture failed: 0x{:08X}", hr as u32);
        assert_eq!(arch, 2, "expected x64");
    }
    eprintln!("Double object table chaining test PASSED");
}

/// Round-trip: read the real saved state VMRS, write it back with our writer,
/// and verify the DLL can load the result.
#[test]
fn roundtrip_real_vmrs_with_dll() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: DLL not loadable");
        return;
    }

    let ref_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");

    if !ref_path.exists() {
        eprintln!("SKIP: saved state VMRS not found at {}", ref_path.display());
        return;
    }

    // Read all keys from the real file
    let file = std::fs::File::open(&ref_path).unwrap();
    let mut reader = hvs_file::reader::HvsFileReader::open(file).unwrap();

    let all_keys: Vec<String> = reader.keys().map(|s| s.to_string()).collect();
    eprintln!("Reference file has {} keys", all_keys.len());

    // Categorize keys by type and read their data
    let buf = Cursor::new(Vec::new());
    let mut w = HvsFileWriter::new(buf).unwrap();

    for key in &all_keys {
        copy_key(&mut reader, &mut w, key);
    }

    let buf = w.finish().unwrap();
    let vmrs_data = buf.into_inner();

    let vmrs_path = std::env::temp_dir().join("hvs_roundtrip_full_test.vmrs");
    std::fs::write(&vmrs_path, &vmrs_data).unwrap();
    let _cleanup = defer(|| {
        let _ = std::fs::remove_file(&vmrs_path);
    });

    eprintln!("Wrote round-tripped file to {} ({} bytes)", vmrs_path.display(), vmrs_data.len());

    // Always save a copy for comparison
    let save_path = std::env::temp_dir().join("hvs_roundtrip_LATEST.vmrs");
    std::fs::copy(&vmrs_path, &save_path).ok();

    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);

        eprintln!("Round-trip DLL load: HRESULT = 0x{:08X}", hr as u32);

        if hr < 0 {
            let debug_path = std::env::temp_dir().join("hvs_roundtrip_FAILED.vmrs");
            std::fs::copy(&vmrs_path, &debug_path).ok();
            panic!(
                "LoadSavedStateFile failed on round-tripped file: HRESULT 0x{:08X}\n\
                 Debug copy at {}",
                hr as u32,
                debug_path.display(),
            );
        }

        let _release = defer(|| {
            dll::ReleaseSavedStateFiles(handle);
        });

        let mut vp_count: u32 = 0;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: 0x{:08X}", hr as u32);
        eprintln!("VP count: {vp_count}");

        let mut arch: u32 = 0;
        let hr = dll::GetArchitecture(handle, 0, &mut arch);
        assert!(hr >= 0, "GetArchitecture failed: 0x{:08X}", hr as u32);
        let arch_name = match arch {
            1 => "x86",
            2 => "x64",
            3 => "ARM64",
            _ => "unknown",
        };
        eprintln!("Round-trip PASSED: VP count = {vp_count}, arch = {arch_name}");
    }
}
