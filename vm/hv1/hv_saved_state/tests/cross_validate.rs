// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-validation test: builds a .vmrs file using `hv_saved_state`, then
//! opens it with VmSavedStateDumpProvider.dll from the Windows SDK and
//! verifies VP count and architecture.
//!
//! Skips gracefully if the DLL is not found on the system.

#![cfg(windows)]
#![allow(unsafe_code)]

use hv_saved_state::PartitionStateBuilder;
use hv_saved_state::ProcessorArch;
use hv_saved_state::VmrsWriter;
use hv_saved_state::VpState;
use hv_saved_state::X64VpState;
use hvdef::Vtl;
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

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
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

/// Build a VMRS file using the hv_saved_state high-level API.
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
                    debug_registers: None,
                    xsave: None,
                }),
            )],
            Vtl::Vtl0,
        );
    }

    let blob = builder.finish();

    let buf = Cursor::new(Vec::new());
    let mut vmrs = VmrsWriter::new(buf).unwrap();
    vmrs.set_partition_state(blob);

    // One 4K page of zeros for RAM
    vmrs.add_memory_range(0, 4096);

    struct ZeroReader;
    impl hv_saved_state::GuestMemoryReader for ZeroReader {
        fn read_gpa(&mut self, _gpa: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            Ok(())
        }
    }
    let mut mem = ZeroReader;
    vmrs.finish(&mut mem).unwrap().into_inner()
}

/// Load a VMRS file with the DLL and verify VP count and architecture.
fn load_and_verify(vmrs_data: &[u8], expected_vp_count: u32, test_name: &str) {
    let vmrs_path = std::env::temp_dir().join(format!("hv_saved_state_{test_name}.vmrs"));
    std::fs::write(&vmrs_path, vmrs_data).unwrap();

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
            "LoadSavedStateFile failed: 0x{:08X}",
            hr as u32
        );
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

    let _ = std::fs::remove_file(&vmrs_path);
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
