// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP primitive operation performance tests.

#[cfg(all(windows, target_arch = "x86_64"))]
// UNSAFETY: Manual memory management to prepare the testing environment and
// calling WHP APIs.
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
mod windows {
    use criterion::BenchmarkId;
    use criterion::Criterion;
    use criterion::Throughput;
    use criterion::criterion_group;
    use std::os::windows::io::RawHandle;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Memory::MEM_COMMIT;
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    use windows_sys::Win32::System::Memory::VirtualAlloc;
    use windows_sys::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::System::Threading::SetEvent;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    const MAP_RWX: whp::abi::WHV_MAP_GPA_RANGE_FLAGS = whp::abi::WHV_MAP_GPA_RANGE_FLAGS(
        whp::abi::WHvMapGpaRangeFlagRead.0
            | whp::abi::WHvMapGpaRangeFlagWrite.0
            | whp::abi::WHvMapGpaRangeFlagExecute.0,
    );

    fn mmio_exit(c: &mut Criterion) {
        let mut pc = whp::PartitionConfig::new().unwrap();

        pc.set_property(whp::PartitionProperty::ProcessorCount(1))
            .unwrap();

        let p = pc.create().unwrap();

        p.create_vp(0).create().unwrap();
        let vp = p.vp(0);

        let mut runner = vp.runner();
        c.bench_function("mmio_exit", |b| {
            b.iter(|| {
                let exit = runner.run().unwrap();
                match exit.reason {
                    whp::ExitReason::MemoryAccess(_) => (),
                    _ => panic!("{:?}", exit),
                }
            })
        });
    }

    fn noapic_interrupt(c: &mut Criterion) {
        let mut pc = whp::PartitionConfig::new().unwrap();

        pc.set_property(whp::PartitionProperty::ProcessorCount(1))
            .unwrap();

        let p = pc.create().unwrap();

        p.create_vp(0).create().unwrap();

        let memp = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        let stackp = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        {
            let mem = unsafe { std::slice::from_raw_parts_mut(memp, 4096) };
            /*
                a: mov byte ptr [bx], al
                hlt
                jmp a
            */
            let code = b"\x88\x07\xf4\xeb\xfb";
            mem[0xff0..0xff0 + code.len()].copy_from_slice(code);
            /*
                iret
            */
            let icode = b"\xcf";
            mem[0xf00..0xf00 + icode.len()].copy_from_slice(icode);
        }
        {
            let mem16 = unsafe { std::slice::from_raw_parts_mut(memp.cast::<u16>(), 2048) };
            mem16[64 * 2] = 0xff00;
            mem16[64 * 2 + 1] = 0xf000;
        }
        unsafe {
            p.map_range(None, memp, 4096, 0xff000, MAP_RWX).unwrap();
            p.map_range(None, memp, 4096, 0, MAP_RWX).unwrap();
            p.map_range(None, stackp, 4096, 0xf000, MAP_RWX).unwrap();
        }

        let vp = p.vp(0);
        whp::set_registers!(
            vp,
            [
                (whp::Register64::Rax, 1),
                (whp::Register64::Rbx, 0x400),
                (whp::Register64::Rflags, 0x202)
            ]
        )
        .unwrap();

        let cell: &AtomicU8 = unsafe { &*(memp.add(0x400) as *const AtomicU8) };

        let mut runner = vp.runner();
        c.bench_function("hlt_exit", |b| {
            b.iter(|| {
                let exit = runner.run().unwrap();
                match exit.reason {
                    whp::ExitReason::Halt => assert_eq!(cell.swap(0, Ordering::Relaxed), 1),
                    _ => panic!("{:#x?}", exit),
                }
            })
        });
        c.bench_function("hlt_exit_then_interrupt", |b| {
            b.iter(|| {
                let exit = runner.run().unwrap();
                match exit.reason {
                    whp::ExitReason::Halt => {
                        assert_eq!(cell.swap(0, Ordering::Relaxed), 1);
                        whp::set_registers!(
                            vp,
                            [(whp::Register64::PendingInterruption, 0x1 | (64 << 16))]
                        )
                        .unwrap();
                    }
                    _ => panic!("{:#x?}", exit),
                }
            })
        });
    }

    fn set_32bit_mode(vp: &whp::Processor<'_>) {
        let cs = whp::abi::WHV_X64_SEGMENT_REGISTER {
            Base: 0,
            Limit: 0xffffffff,
            Selector: 0x8,
            Attributes: 0xcf9b,
        };

        let ds = whp::abi::WHV_X64_SEGMENT_REGISTER {
            Base: 0,
            Limit: 0xffffffff,
            Selector: 0x10,
            Attributes: 0xcf93,
        };

        whp::set_registers!(
            vp,
            [
                (whp::RegisterSegment::Cs, cs),
                (whp::RegisterSegment::Ds, ds),
                (whp::RegisterSegment::Es, ds),
                (whp::RegisterSegment::Fs, ds),
                (whp::RegisterSegment::Gs, ds),
                (whp::RegisterSegment::Ss, ds),
                (whp::Register64::Cr0, 1),
            ]
        )
        .unwrap();
    }

    fn event_signal(c: &mut Criterion) {
        let ea = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
        let eb = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };

        {
            let an = ea as usize;
            let bn = eb as usize;
            std::thread::spawn(move || {
                loop {
                    unsafe {
                        assert!(WaitForSingleObject(an as RawHandle, 0xffffffff) == 0);
                        assert!(SetEvent(bn as RawHandle) != 0);
                    }
                }
            })
        };

        c.bench_function("event_roundtrip_no_direct_switch", |b| {
            b.iter(|| unsafe {
                assert!(SetEvent(ea) != 0);
                assert!(WaitForSingleObject(eb, 0xffffffff) == 0);
            });
        });
    }

    fn apic_interrupt(c: &mut Criterion) {
        let mut pc = whp::PartitionConfig::new().unwrap();

        pc.set_property(whp::PartitionProperty::ProcessorCount(1))
            .unwrap()
            .set_property(whp::PartitionProperty::LocalApicEmulationMode(
                whp::abi::WHvX64LocalApicEmulationModeXApic,
            ))
            .unwrap();

        let p = Arc::new(pc.create().unwrap());
        p.create_vp(0).create().unwrap();

        let codep = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        let datap = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        let stackp = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        let tablep = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        {
            let mem = unsafe { std::slice::from_raw_parts_mut(codep, 4096) };
            /*
                mov dword ptr 0xf0[ecx], 0x1ff /* configure APIC */
                a:
                sti
                hlt
                cli
                mov byte ptr [ebx], al
                jmp a
            */
            let code = b"\xc7\x81\xf0\x00\x00\x00\xff\x01\x00\x00\xfb\xf4\xfa\x88\x03\xeb\xf9";
            mem[..code.len()].copy_from_slice(code);

            /*
                mov dword ptr 0xf0[ecx], 0x1ff /* configure APIC */
                sti
                a:
                pause
                jmp a
            */
            let tight_loop_code = b"\xc7\x81\xf0\x00\x00\x00\xff\x01\x00\x00\xfb\xf3\x90\xeb\xfc";
            mem[TIGHT_LOOP_OFFSET..TIGHT_LOOP_OFFSET + tight_loop_code.len()]
                .copy_from_slice(tight_loop_code);

            /*
                mov dword ptr 0xb0[ecx], 0 /* apic EOI */
                iretd
            */
            let icode = b"\xc7\x81\xb0\x00\x00\x00\x00\x00\x00\x00\xcf";
            mem[ISR_OFFSET..ISR_OFFSET + icode.len()].copy_from_slice(icode);

            /*
                mov dword ptr 0xb0[ecx], 0 /* apic EOI */
                mov byte ptr [ebx], al
                iretd
            */
            let iscode = b"\xc7\x81\xb0\x00\x00\x00\x00\x00\x00\x00\x88\x03\xcf";
            mem[SIGNAL_ISR_OFFSET..SIGNAL_ISR_OFFSET + iscode.len()].copy_from_slice(iscode);
        }
        {
            #[repr(C)]
            #[derive(Clone, Copy, Default)]
            pub struct GdtEntry {
                pub limit_low: u16,
                pub base_low: u16,
                pub base_middle: u8,
                pub attr_low: u8,
                pub attr_high: u8,
                pub base_high: u8,
            }

            let gdt = unsafe {
                std::slice::from_raw_parts_mut(tablep.add(GDT_OFFSET).cast::<GdtEntry>(), 4)
            };
            gdt.copy_from_slice(&[
                Default::default(),
                GdtEntry {
                    limit_low: 0xffff,
                    attr_low: 0x9b,
                    attr_high: 0xcf,
                    ..Default::default()
                },
                GdtEntry {
                    limit_low: 0xffff,
                    attr_low: 0x93,
                    attr_high: 0xcf,
                    ..Default::default()
                },
                Default::default(),
            ]);

            #[repr(C)]
            #[derive(Clone, Copy, Default)]
            pub struct IdtEntry {
                pub offset_low: u16,
                pub selector: u16,
                pub zero: u8,
                pub type_attr: u8,
                pub offset_high: u16,
            }

            let idt = unsafe {
                std::slice::from_raw_parts_mut(tablep.add(IDT_OFFSET).cast::<IdtEntry>(), 256)
            };
            fn entry(addr: u64) -> IdtEntry {
                IdtEntry {
                    offset_low: addr as u16,
                    selector: 0x8,
                    zero: 0,
                    type_attr: 0x8e, // present, 32-bit interrupt gate
                    offset_high: (addr >> 16) as u16,
                }
            }
            idt[ISR_VECTOR as usize] = entry(CODE_ADDRESS + ISR_OFFSET as u64);
            idt[SIGNAL_ISR_VECTOR as usize] = entry(CODE_ADDRESS + SIGNAL_ISR_OFFSET as u64);
        }
        unsafe {
            p.map_range(None, codep, 4096, CODE_ADDRESS, MAP_RWX)
                .unwrap();
            p.map_range(None, datap, 4096, DATA_ADDRESS, MAP_RWX)
                .unwrap();
            p.map_range(None, tablep, 4096, TABLE_ADDRESS, MAP_RWX)
                .unwrap();
            p.map_range(None, stackp, 4096, STACK_ADDRESS, MAP_RWX)
                .unwrap();
        }

        let vp = p.vp(0);
        set_32bit_mode(&vp);

        const CODE_ADDRESS: u64 = 0x100000;
        const TIGHT_LOOP_OFFSET: usize = 0x100;
        const ISR_OFFSET: usize = 0x200;
        const SIGNAL_ISR_OFFSET: usize = 0x300;
        const DATA_ADDRESS: u64 = 0x200000;
        const TABLE_ADDRESS: u64 = 0x300000;
        const GDT_OFFSET: usize = 0;
        const IDT_OFFSET: usize = 0x100;
        const STACK_ADDRESS: u64 = 0x400000;
        const APIC_BASE: u64 = 0xfee00000;
        const ISR_VECTOR: u32 = 32;
        const SIGNAL_ISR_VECTOR: u32 = 33;

        whp::set_registers!(
            vp,
            [
                (whp::Register64::Rip, CODE_ADDRESS),
                (whp::Register64::Rsp, STACK_ADDRESS + 0x1000),
                (whp::Register64::Rax, 1),
                (whp::Register64::Rbx, DATA_ADDRESS),
                (whp::Register64::Rcx, APIC_BASE),
                (
                    whp::RegisterTable::Idtr,
                    whp::abi::WHV_X64_TABLE_REGISTER {
                        Base: TABLE_ADDRESS + IDT_OFFSET as u64,
                        Limit: 256 * 8,
                        Pad: [0; 3]
                    }
                ),
                (
                    whp::RegisterTable::Gdtr,
                    whp::abi::WHV_X64_TABLE_REGISTER {
                        Base: TABLE_ADDRESS + GDT_OFFSET as u64,
                        Limit: 4 * 8,
                        Pad: [0; 3]
                    }
                ),
            ]
        )
        .unwrap();

        // Enable the APIC.
        let mut apic = vp.get_apic().unwrap();
        apic[0xf0..0xf4].copy_from_slice(&0x1ffu32.to_ne_bytes());
        vp.set_apic(&apic).unwrap();

        let cell: &AtomicU8 = unsafe { &*(datap as *const AtomicU8) };

        let run = |offset: usize, event: Option<RawHandle>| {
            assert!(cell.load(Ordering::Relaxed) == 0);
            whp::set_registers!(
                vp,
                [
                    (whp::Register64::Rip, CODE_ADDRESS + offset as u64),
                    (whp::Register64::Rflags, 2),
                    (whp::Register64::InternalActivityState, 0),
                ]
            )
            .unwrap();

            let p = p.clone();
            let event = event.map(|e| e as usize);
            std::thread::spawn(move || {
                let vp = p.vp(0);
                let mut runner = vp.runner();
                loop {
                    let exit = runner.run().unwrap();
                    match exit.reason {
                        whp::ExitReason::Canceled => break,
                        whp::ExitReason::MemoryAccess(_) if event.is_some() => {
                            vp.set_register(
                                whp::Register64::Rip,
                                exit.vp_context.Rip.wrapping_add(2) & 0xffffffff,
                            )
                            .unwrap();
                            assert!(
                                unsafe { SetEvent(event.unwrap() as *mut std::ffi::c_void) } != 0
                            );
                        }
                        _ => panic!("{:#x?}", exit),
                    }
                }
            })
        };

        let stop = |thread: std::thread::JoinHandle<()>| {
            vp.cancel_run().unwrap();
            thread.join().unwrap();
            assert!(cell.load(Ordering::Relaxed) == 0);
        };

        let event = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };

        let mut group = c.benchmark_group("apic");
        group.throughput(Throughput::Elements(1));

        group.bench_function("interrupt_stopped", |b| {
            b.iter(|| {
                p.interrupt(
                    whp::abi::WHvX64InterruptTypeFixed,
                    whp::abi::WHvX64InterruptDestinationModePhysical,
                    whp::abi::WHvX64InterruptTriggerModeEdge,
                    0,
                    ISR_VECTOR,
                )
                .unwrap();
            });
            // Drain the pending interrupt.
            let thread = run(0, None);
            while cell.load(Ordering::Acquire) == 0 {}
            cell.store(0, Ordering::Relaxed);
            stop(thread);
        });

        #[derive(PartialEq, Eq, Debug, Copy, Clone)]
        enum Method {
            Poll,
            Emulate,
            Doorbell,
        }

        for halt in &[true, false] {
            for method in &[Method::Poll, Method::Emulate, Method::Doorbell] {
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("interrupt_{}", if *halt { "halted" } else { "running" }),
                        format!("{:?}", &method),
                    ),
                    &(*method, *halt),
                    |b, (method, halt)| {
                        let m = &whp::DoorbellMatch {
                            guest_address: DATA_ADDRESS,
                            length: Some(1),
                            value: None,
                        };

                        match method {
                            Method::Emulate => {
                                p.unmap_range(DATA_ADDRESS, 4096).unwrap();
                            }
                            Method::Doorbell => {
                                unsafe { p.register_doorbell(m, event) }.unwrap();
                            }
                            Method::Poll => (),
                        };

                        let thread = run(
                            if *halt { 0 } else { TIGHT_LOOP_OFFSET },
                            if *method == Method::Emulate {
                                Some(event)
                            } else {
                                None
                            },
                        );

                        let vector = if *halt { ISR_VECTOR } else { SIGNAL_ISR_VECTOR };

                        b.iter(|| {
                            p.interrupt(
                                whp::abi::WHvX64InterruptTypeFixed,
                                whp::abi::WHvX64InterruptDestinationModePhysical,
                                whp::abi::WHvX64InterruptTriggerModeEdge,
                                0,
                                vector,
                            )
                            .unwrap();
                            if *method == Method::Poll {
                                while cell.load(Ordering::Acquire) == 0 {}
                                cell.store(0, Ordering::Relaxed);
                            } else {
                                assert!(unsafe { WaitForSingleObject(event, 0xffffffff) } == 0);
                            }
                        });

                        if !halt {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }

                        stop(thread);

                        if !halt {
                            // If we are still in the interrupt handler then the next test is going
                            // to behave unpredictably.
                            let rip = vp.get_register(whp::Register64::Rip).unwrap();
                            assert!(
                                rip >= CODE_ADDRESS + TIGHT_LOOP_OFFSET as u64
                                    && rip < CODE_ADDRESS + TIGHT_LOOP_OFFSET as u64 + 0x100
                            );
                        }

                        // Restore the partition state.
                        match method {
                            Method::Emulate => {
                                unsafe { p.map_range(None, datap, 4096, DATA_ADDRESS, MAP_RWX) }
                                    .unwrap();
                            }
                            Method::Doorbell => {
                                p.unregister_doorbell(m).unwrap();
                            }
                            Method::Poll => (),
                        }
                    },
                );
            }
        }
    }

    criterion_group!(
        benches,
        mmio_exit,
        noapic_interrupt,
        apic_interrupt,
        event_signal
    );
}

#[cfg(all(windows, target_arch = "x86_64"))]
criterion::criterion_main!(windows::benches);

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn main() {}
