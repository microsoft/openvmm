// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-validation test: builds a .vmrs file using `hyperv_dump`, then
//! opens it with VmSavedStateDumpProvider.dll from the Windows SDK and
//! verifies VP count and architecture.
//!
//! Skips gracefully if the DLL is not found on the system.

#![cfg(windows)]
// UNSAFETY: FFI calls to VmSavedStateDumpProvider.dll for cross-validation.
#![expect(unsafe_code)]

use hvdef::Vtl;
use hyperv_dump::PartitionStateBuilder;
use hyperv_dump::ProcessorArch;
use hyperv_dump::VmrsWriter;
use hyperv_dump::VpState;
use hyperv_dump::X64VpState;
use std::ffi::c_void;
use std::io::Cursor;
use std::path::PathBuf;

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

fn setup_dll_search_path() -> bool {
    let kits_root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if !kits_root.exists() {
        return false;
    }

    // xtask-fmt allow-target-arch sys-crate
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    // xtask-fmt allow-target-arch sys-crate
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        return false;
    };

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

fn zero_xsave() -> virt::x86::vp::Xsave {
    virt::x86::vp::Xsave {
        data: vec![0u64; 72],
    }
}

/// Build a VMRS file using the hyperv_dump API.
fn build_vmrs_via_builder(rip: u64, cr3: u64, vp_count: u32) -> Vec<u8> {
    let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
    builder.set_os_id(0);

    for i in 0..vp_count {
        let mut regs = virt::x86::vp::Registers::default();
        regs.rip = rip + i as u64;
        regs.rsp = 0xFFFFF780_00000000;
        regs.rax = 0xDEAD_BEEF;
        regs.cr0 = 0x80050033;
        regs.cr3 = cr3;
        regs.cr4 = 0x370678;
        regs.efer = 0xD01;
        regs.cs = virt::x86::SegmentRegister {
            base: 0,
            limit: 0xFFFFFFFF,
            selector: 0x10,
            attributes: 0x209B,
        };
        regs.idtr = virt::x86::TableRegister {
            base: 0xFFFFF800_00000000,
            limit: 0xFFF,
        };
        regs.gdtr = virt::x86::TableRegister {
            base: 0xFFFFF800_00001000,
            limit: 0x6F,
        };
        builder.add_vp(
            i,
            vec![(
                Vtl::Vtl0,
                VpState::X64(X64VpState {
                    registers: regs,
                    debug_registers: Default::default(),
                    xsave: zero_xsave(),
                    xcr0: virt::x86::vp::Xcr0 { value: 1 },
                }),
            )],
        );
    }

    let blob = builder.finish();

    let buf = Cursor::new(Vec::new());
    let mut vmrs = VmrsWriter::new(buf).unwrap();
    vmrs.set_partition_state(blob);

    // One 4K page of zeros for RAM
    vmrs.add_memory_range(memory_range::MemoryRange::new(0..4096));

    struct ZeroReader;
    impl hyperv_dump::GuestMemoryReader for ZeroReader {
        fn read_gpa(&mut self, _gpa: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            Ok(())
        }
    }
    let mut mem = ZeroReader;
    vmrs.finish(&mut mem).unwrap().into_inner()
}

/// Load a VMRS file with the DLL and verify VP count and architecture.
fn load_and_verify(vmrs_data: &[u8], expected_vp_count: u32, _test_name: &str) {
    let mut tmp = tempfile::Builder::new().suffix(".vmrs").tempfile().unwrap();
    std::io::Write::write_all(&mut tmp, vmrs_data).unwrap();

    let vmrs_path = tmp.path();
    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);
        assert!(hr >= 0, "LoadSavedStateFile failed: 0x{:08X}", hr as u32);
        assert!(!handle.is_null());

        // Verify VP count
        let mut vp_count = 0u32;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: 0x{:08X}", hr as u32);
        assert_eq!(
            vp_count, expected_vp_count,
            "VP count mismatch: got {vp_count}, expected {expected_vp_count}"
        );

        // Verify architecture
        // VIRTUAL_PROCESSOR_ARCH: Arch_x64 = 2
        let mut arch = 0u32;
        let hr = dll::GetArchitecture(handle, 0, &mut arch);
        assert!(hr >= 0, "GetArchitecture failed: 0x{:08X}", hr as u32);
        assert_eq!(arch, 2, "Expected Arch_x64 (2), got {arch}");

        dll::ReleaseSavedStateFiles(handle);
    }
    // tmp is dropped here, cleaning up the file automatically
}

#[test]
fn dll_validates_single_vp() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: VmSavedStateDumpProvider.dll not loadable");
        return;
    }

    let vmrs = build_vmrs_via_builder(0xFFFFF800_12345678, 0x1AD000, 1);
    eprintln!("Built VMRS file: {} bytes", vmrs.len());
    load_and_verify(&vmrs, 1, "single_vp");
    eprintln!("Single VP validation PASSED");
}

#[test]
fn dll_validates_multi_vp() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: VmSavedStateDumpProvider.dll not loadable");
        return;
    }

    let vmrs = build_vmrs_via_builder(0xFFFFF800_12345678, 0x1AD000, 4);
    eprintln!("Built 4-VP VMRS file: {} bytes", vmrs.len());
    load_and_verify(&vmrs, 4, "multi_vp");
    eprintln!("Multi-VP validation PASSED");
}

/// Test with enough memory blocks to force object table chaining.
/// 500 × 1 MiB blocks = 500 file objects + key tables → requires 3
/// chained object tables (226 usable entries per table).
#[test]
fn dll_validates_large_memory() {
    if !setup_dll_search_path() {
        eprintln!("SKIP: Windows SDK not found");
        return;
    }
    if !dll::is_supported::LoadSavedStateFile() {
        eprintln!("SKIP: VmSavedStateDumpProvider.dll not loadable");
        return;
    }

    let mut builder = PartitionStateBuilder::new(ProcessorArch::X64);
    builder.set_os_id(0);

    let mut regs = virt::x86::vp::Registers::default();
    regs.rip = 0xFFFFF800_12345678;
    regs.cr3 = 0x1AD000;
    regs.cr0 = 0x80050033;
    regs.efer = 0xD01;
    regs.cs = virt::x86::SegmentRegister {
        base: 0,
        limit: 0xFFFFFFFF,
        selector: 0x10,
        attributes: 0x209B,
    };
    builder.add_vp(
        0,
        vec![(
            Vtl::Vtl0,
            VpState::X64(X64VpState {
                registers: regs,
                debug_registers: Default::default(),
                xsave: zero_xsave(),
                xcr0: virt::x86::vp::Xcr0 { value: 1 },
            }),
        )],
    );
    let blob = builder.finish();

    let buf = Cursor::new(Vec::new());
    let mut vmrs = VmrsWriter::new(buf).unwrap();
    vmrs.set_partition_state(blob);

    // 500 MiB of memory at GPA 0 — 500 × 1 MiB blocks.
    const BLOCK_COUNT: u64 = 500;
    const MIB: u64 = 1_048_576;
    vmrs.add_memory_range(memory_range::MemoryRange::new(0..BLOCK_COUNT * MIB));

    /// Reader that fills each block with a stamp derived from the GPA.
    struct StampReader;
    impl hyperv_dump::GuestMemoryReader for StampReader {
        fn read_gpa(&mut self, gpa: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            // Stamp the first 8 bytes with the GPA for verification.
            let stamp = gpa.to_le_bytes();
            buf[..8].copy_from_slice(&stamp);
            Ok(())
        }
    }

    let mut mem = StampReader;
    let vmrs_data = vmrs.finish(&mut mem).unwrap().into_inner();
    eprintln!(
        "Built large VMRS: {} bytes ({} MiB, {} blocks)",
        vmrs_data.len(),
        vmrs_data.len() / 1_048_576,
        BLOCK_COUNT
    );

    // Verify round-trip through our reader: spot-check a few blocks.
    {
        let mut reader = hvs_file::reader::HvsFileReader::open(Cursor::new(&vmrs_data)).unwrap();
        for i in [0u64, 1, 249, 250, 499] {
            let data = reader
                .read_array(&format!("/savedstate/RamBlock{i}"))
                .unwrap();
            assert_eq!(data.len(), MIB as usize, "RamBlock{i} wrong size");
            let stamp = u64::from_le_bytes(data[..8].try_into().unwrap());
            assert_eq!(stamp, i * MIB, "RamBlock{i} GPA stamp mismatch");
        }
    }

    // Verify the DLL can load the file.
    let mut tmp = tempfile::Builder::new().suffix(".vmrs").tempfile().unwrap();
    std::io::Write::write_all(&mut tmp, &vmrs_data).unwrap();

    let vmrs_path = tmp.path();
    let wide_path: Vec<u16> = vmrs_path
        .to_str()
        .unwrap()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = dll::LoadSavedStateFile(wide_path.as_ptr(), &mut handle);
        assert!(
            hr >= 0,
            "LoadSavedStateFile failed on large file: 0x{:08X}",
            hr as u32
        );
        assert!(!handle.is_null());

        let mut vp_count = 0u32;
        let hr = dll::GetVpCount(handle, &mut vp_count);
        assert!(hr >= 0, "GetVpCount failed: 0x{:08X}", hr as u32);
        assert_eq!(vp_count, 1);

        dll::ReleaseSavedStateFiles(handle);
    }

    eprintln!("Large memory ({BLOCK_COUNT} blocks) validation PASSED");
}
