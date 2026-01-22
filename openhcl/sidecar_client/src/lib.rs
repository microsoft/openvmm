// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The client interface to the sidecar kernel driver.

#![cfg(target_os = "linux")]
// UNSAFETY: Manually mapping memory for the sidecar kernel and calling ioctls.
#![expect(unsafe_code)]

use fs_err::os::unix::fs::OpenOptionsExt;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvStatus;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::hypercall::TranslateVirtualAddressExOutputX64;
use pal_async::driver::PollImpl;
use pal_async::driver::SpawnDriver;
use pal_async::fd::PollFdReady;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::task::Task;
use parking_lot::Mutex;
use sidecar_defs::CommandPage;
use sidecar_defs::CpuContextX64;
use sidecar_defs::GetSetVpRegisterRequest;
use sidecar_defs::PAGE_SIZE;
use sidecar_defs::RunVpResponse;
use sidecar_defs::SidecarCommand;
use sidecar_defs::TranslateGvaRequest;
use sidecar_defs::TranslateGvaResponse;
use std::fs::File;
use std::future::poll_fn;
use std::io::Read;
use std::mem::MaybeUninit;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::raw::c_void;
use std::ptr::NonNull;
use std::ptr::addr_of;
use std::ptr::addr_of_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

mod ioctl {
    const BASE: u8 = 0xb8;
    nix::ioctl_write_int_bad!(mshv_vtl_sidecar_start, nix::request_code_none!(BASE, 0xf0));
    nix::ioctl_write_int_bad!(mshv_vtl_sidecar_stop, nix::request_code_none!(BASE, 0xf1));
    nix::ioctl_write_int_bad!(mshv_vtl_sidecar_run, nix::request_code_none!(BASE, 0xf2));
    nix::ioctl_read!(mshv_vtl_sidecar_info, BASE, 0xf3, SidecarInfo);

    #[repr(C)]
    pub(crate) struct SidecarInfo {
        pub base_cpu: u32,
        pub cpu_count: u32,
        pub per_cpu_shmem: u32,
    }
}

/// A sidecar client.
///
/// This is actually a client to multiple sidecar devices, since there is one
/// per node. This is abstracted away for the caller.
#[derive(Debug)]
pub struct SidecarClient {
    nodes: Vec<SidecarNode>,
}

#[derive(Debug)]
struct SidecarNode {
    mapping: Mapping,
    per_cpu_shmem_size: usize,
    cpus: Range<u32>,
    _task: Task<()>,
    state: Arc<SidecarClientState>,
    in_use: Vec<AtomicBool>,
}

#[derive(Debug)]
struct SidecarClientState {
    file: File,
    vps: Vec<Mutex<VpState>>,
}

#[derive(Debug)]
enum VpState {
    Stopped,
    Running(Option<Waker>),
    Finished,
}

#[derive(Debug)]
struct Mapping(NonNull<c_void>, usize);

// SAFETY: the underlying mapping can be accessed from any CPU.
unsafe impl Send for Mapping {}
// SAFETY: the underlying mapping can be accessed from any CPU.
unsafe impl Sync for Mapping {}

/// An error returned by [`SidecarClient::new`].
#[derive(Debug, Error)]
pub enum NewSidecarClientError {
    /// IO failure interacting with the sidecar driver.
    #[error("{operation} failed in sidecar driver")]
    Io {
        /// The IO operation.
        operation: &'static str,
        /// The error.
        #[source]
        err: std::io::Error,
    },
    /// An error from an IO driver.
    #[error("driver error")]
    Driver(#[source] std::io::Error),
}

impl SidecarClient {
    /// Create a new sidecar client. Returns `None` if no sidecar devices are found.
    ///
    /// `driver(cpu)` returns the driver to use for polling the sidecar device
    /// whose base CPU is `cpu`.
    pub fn new<T: SpawnDriver>(
        mut driver: impl FnMut(u32) -> T,
    ) -> Result<Option<Self>, NewSidecarClientError> {
        let mut nodes = Vec::new();
        let mut expected_base = 0;
        loop {
            let node = match SidecarNode::new(&mut driver, nodes.len()) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    if nodes.is_empty() {
                        // No sidecar devices could be found at all.
                        return Ok(None);
                    }
                    // No more nodes.
                    break;
                }
                Err(err) => return Err(err),
            };
            assert_eq!(node.cpus.start, expected_base);
            expected_base = node.cpus.end;
            nodes.push(node);
        }
        Ok(Some(Self { nodes }))
    }

    /// Returns a sidecar VP accessor for the given CPU.
    pub fn vp(&self, cpu: u32) -> SidecarVp<'_> {
        self.nodes
            .iter()
            .find_map(|node| node.vp(cpu))
            .expect("invalid cpu")
    }

    /// Returns the CPU index that manages the given VP.
    pub fn base_cpu(&self, cpu: u32) -> u32 {
        self.nodes
            .iter()
            .find_map(|node| node.cpus.contains(&cpu).then_some(node.cpus.start))
            .expect("invalid cpu")
    }
}

impl SidecarNode {
    fn new<T: SpawnDriver>(
        driver: &mut impl FnMut(u32) -> T,
        node: usize,
    ) -> Result<Option<Self>, NewSidecarClientError> {
        let file = match fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(format!("/dev/mshv_vtl_sidecar{node}"))
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(NewSidecarClientError::Io {
                    operation: "open",
                    err,
                });
            }
        };

        // SAFETY: calling the ioctl with a valid output pointer. The ioctl is
        // guaranteed to initialize the output on success (but pre-zero it just to be safe).
        let info = unsafe {
            let mut info = MaybeUninit::zeroed();
            ioctl::mshv_vtl_sidecar_info(file.as_raw_fd(), info.as_mut_ptr()).map_err(|err| {
                NewSidecarClientError::Io {
                    operation: "query info",
                    err: err.into(),
                }
            })?;
            info.assume_init()
        };

        let cpus = info.base_cpu..info.base_cpu + info.cpu_count;
        let per_cpu_shmem_size = info.per_cpu_shmem as usize;
        assert!(
            per_cpu_shmem_size >= size_of::<VpSharedPages>(),
            "invalid state size"
        );

        let mapping = {
            let mapping_len = cpus.len() * per_cpu_shmem_size;
            // SAFETY: creating a new mapping, which has no safety requirements.
            let mapping = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    mapping_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            if mapping == libc::MAP_FAILED {
                return Err(NewSidecarClientError::Io {
                    operation: "mmap",
                    err: std::io::Error::last_os_error(),
                });
            }
            Mapping(NonNull::new(mapping).unwrap(), mapping_len)
        };

        // Start the driver on the first CPU in the node.
        let driver = driver(cpus.start);

        let fd_ready = driver
            .new_dyn_fd_ready(file.as_raw_fd())
            .map_err(NewSidecarClientError::Driver)?;

        let state = Arc::new(SidecarClientState {
            file: file.into(),
            vps: cpus.clone().map(|_| Mutex::new(VpState::Stopped)).collect(),
        });

        let task = driver.spawn(
            "sidecar-wait",
            sidecar_wait_loop(fd_ready, state.clone(), cpus.start),
        );

        tracing::debug!(
            "sidecar node {node} started, cpus {}..={}",
            cpus.start,
            cpus.end - 1
        );

        Ok(Some(Self {
            state,
            per_cpu_shmem_size,
            mapping,
            in_use: cpus.clone().map(|_| AtomicBool::new(false)).collect(),
            cpus,
            _task: task,
        }))
    }

    fn vp(&self, cpu: u32) -> Option<SidecarVp<'_>> {
        if !self.cpus.contains(&cpu) {
            return None;
        }
        let index = cpu - self.cpus.start;
        assert!(
            !self.in_use[index as usize].swap(true, Acquire),
            "vp in use"
        );
        // SAFETY: the mapping is valid and the index is within the range of CPUs.
        let shmem = unsafe {
            self.mapping
                .0
                .as_ptr()
                .byte_add(index as usize * self.per_cpu_shmem_size)
        }
        .cast();
        Some(SidecarVp {
            cpu: cpu as i32,
            index: index as usize,
            shmem: NonNull::new(shmem).unwrap(),
            node: self,
        })
    }
}

async fn sidecar_wait_loop(
    mut fd_ready: PollImpl<dyn PollFdReady>,
    state: Arc<SidecarClientState>,
    base_cpu: u32,
) {
    let err = loop {
        poll_fn(|cx| fd_ready.poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN)).await;
        let mut cpu = 0u32;
        let n = match (&state.file).read(cpu.as_mut_bytes()) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                fd_ready.clear_fd_ready(InterestSlot::Read);
                continue;
            }
            Err(err) => break err,
        };
        assert_eq!(n, 4, "unexpected read size");
        tracing::trace!(cpu, "sidecar stop");
        let index = cpu - base_cpu;
        let VpState::Running(waker) =
            std::mem::replace(&mut *state.vps[index as usize].lock(), VpState::Finished)
        else {
            panic!("cpu {cpu} stopped without start");
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    };
    tracing::error!(
        error = &err as &dyn std::error::Error,
        "sidecar wait failed"
    );
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping is valid and the length is correct.
        let r = unsafe { libc::munmap(self.0.as_ptr(), self.1) };
        if r != 0 {
            panic!("munmap failed: {}", std::io::Error::last_os_error());
        }
    }
}

/// An accessor for a sidecar VP.
pub struct SidecarVp<'a> {
    cpu: i32,
    index: usize,
    shmem: NonNull<VpSharedPages>,
    node: &'a SidecarNode,
}

#[repr(C)]
struct VpSharedPages {
    command_page: CommandPage,
    register_page: hvdef::HvX64RegisterPage,
}

const _: () = assert!(size_of::<VpSharedPages>().is_multiple_of(PAGE_SIZE));

impl Drop for SidecarVp<'_> {
    fn drop(&mut self) {
        assert!(self.node.in_use[self.index].swap(false, Release));
    }
}

/// An error from a sidecar operation.
#[derive(Debug, Error)]
pub enum SidecarError {
    /// An IO error interacting with the sidecar driver.
    #[error("driver error")]
    Io(#[source] std::io::Error),
    /// An error from the sidecar kernel.
    #[error("sidecar error: {0}")]
    Sidecar(String),
    /// An error from the hypervisor.
    #[error("hypervisor error")]
    Hypervisor(#[source] HvError),
}

impl<'a> SidecarVp<'a> {
    /// Runs the VP.
    pub fn run(&mut self) -> Result<SidecarRun<'_, 'a>, SidecarError> {
        tracing::trace!("run vp");
        self.set_command::<_, u8>(SidecarCommand::RUN_VP, (), 0);
        self.start_async()?;
        Ok(SidecarRun {
            vp: self,
            waited: false,
        })
    }

    /// Returns a pointer to the CPU context.
    ///
    /// This pointer is only valid for access while the VP is stopped.
    pub fn cpu_context(&self) -> *mut CpuContextX64 {
        // SAFETY: the command page pointer is valid so these pointer computations
        // are also valid.
        unsafe { addr_of_mut!((*self.shmem.as_ptr()).command_page.cpu_context) }
    }

    /// Returns a pointer to the intercept message from the hypervisor.
    ///
    /// This pointer is only valid for access while the VP is stopped.
    pub fn intercept_message(&self) -> *const HvMessage {
        // SAFETY: the command page pointer is valid so these pointer computations
        // are also valid.
        unsafe { addr_of!((*self.shmem.as_ptr()).command_page.intercept_message) }
    }

    /// Returns a pointer to the register page, mapped with the hypervisor.
    ///
    /// If the hypervisor does not support register pages, then the `is_valid`
    /// field will be 0.
    ///
    /// This pointer is only valid for access while the VP is stopped.
    pub fn register_page(&self) -> *mut hvdef::HvX64RegisterPage {
        // SAFETY: the command page pointer is valid so these pointer computations
        // are also valid.
        unsafe { addr_of_mut!((*self.shmem.as_ptr()).register_page) }
    }

    /// Tests that the VP is running in the sidecar kernel.
    pub fn test(&mut self) -> Result<(), SidecarError> {
        tracing::trace!("test");
        let () = self.dispatch_sync(SidecarCommand::NONE, ())?;
        Ok(())
    }

    /// Gets a VP register by name.
    pub fn get_vp_registers(
        &mut self,
        target_vtl: HvInputVtl,
        names: &[HvRegisterName],
        values: &mut [HvRegisterValue],
    ) -> Result<(), SidecarError> {
        tracing::trace!(count = names.len(), "get vp register");
        for (names, values) in names
            .chunks(sidecar_defs::MAX_GET_SET_VP_REGISTERS)
            .zip(values.chunks_mut(sidecar_defs::MAX_GET_SET_VP_REGISTERS))
        {
            let buf = self.set_command(
                SidecarCommand::GET_VP_REGISTERS,
                GetSetVpRegisterRequest {
                    count: names.len() as u16,
                    target_vtl,
                    rsvd: 0,
                    status: HvStatus::SUCCESS,
                    rsvd2: [0; 10],
                    regs: [],
                },
                names.len(),
            );
            for (i, name) in names.iter().enumerate() {
                buf[i] = HvRegisterAssoc {
                    name: *name,
                    pad: Default::default(),
                    value: FromZeros::new_zeroed(),
                };
            }
            self.run_sync()?;
            let (&GetSetVpRegisterRequest { status, .. }, buf) =
                self.command_result::<_, HvRegisterAssoc>(names.len())?;
            status.result().map_err(SidecarError::Hypervisor)?;
            for (i, value) in values.iter_mut().enumerate() {
                *value = buf[i].value;
            }
        }
        Ok(())
    }

    /// Sets a VP register by name.
    pub fn set_vp_registers(
        &mut self,
        target_vtl: HvInputVtl,
        regs: &[HvRegisterAssoc],
    ) -> Result<(), SidecarError> {
        tracing::trace!(count = regs.len(), "set vp register");
        for regs in regs.chunks(sidecar_defs::MAX_GET_SET_VP_REGISTERS) {
            let buf = self.set_command(
                SidecarCommand::SET_VP_REGISTERS,
                GetSetVpRegisterRequest {
                    count: regs.len() as u16,
                    target_vtl,
                    rsvd: 0,
                    status: HvStatus::SUCCESS,
                    rsvd2: [0; 10],
                    regs: [],
                },
                regs.len(),
            );
            buf.copy_from_slice(regs);
            self.run_sync()?;
            let &GetSetVpRegisterRequest { status, .. } = self.command_result::<_, u8>(0)?.0;
            status.result().map_err(SidecarError::Hypervisor)?;
        }
        Ok(())
    }

    /// Issues a hypercall to translate a guest virtual address to a guest
    /// physical address.
    pub fn translate_gva(
        &mut self,
        gvn: u64,
        control_flags: hvdef::hypercall::TranslateGvaControlFlagsX64,
    ) -> Result<TranslateVirtualAddressExOutputX64, SidecarError> {
        tracing::trace!("translate gva");
        let &TranslateGvaResponse {
            status,
            rsvd: _,
            output,
        } = self.dispatch_sync(
            SidecarCommand::TRANSLATE_GVA,
            TranslateGvaRequest { gvn, control_flags },
        )?;
        status.result().map_err(SidecarError::Hypervisor)?;
        Ok(output)
    }

    fn set_command<
        T: IntoBytes + Immutable + KnownLayout,
        S: IntoBytes + FromBytes + Immutable + KnownLayout,
    >(
        &mut self,
        command: SidecarCommand,
        input: T,
        n: usize,
    ) -> &mut [S] {
        // SAFETY: no command is running, so the sidecar kernel will not
        // concurrently modify the state page.
        let shmem = unsafe { self.shmem.as_mut() };
        shmem.command_page.command = command;
        input
            .write_to_prefix(shmem.command_page.request_data.as_mut_bytes())
            .unwrap();
        <[S]>::mut_from_prefix_with_elems(
            &mut shmem.command_page.request_data.as_mut_bytes()[input.as_bytes().len()..],
            n,
        )
        .unwrap()
        .0
    }

    fn dispatch_sync<O: FromBytes + Immutable + KnownLayout>(
        &mut self,
        command: SidecarCommand,
        input: impl IntoBytes + Immutable + KnownLayout,
    ) -> Result<&O, SidecarError> {
        self.set_command::<_, u8>(command, input, 0);
        self.run_sync()?;
        Ok(self.command_result::<_, u8>(0)?.0)
    }

    fn run_sync(&mut self) -> Result<(), SidecarError> {
        // SAFETY: no safety requirements on this ioctl.
        unsafe {
            ioctl::mshv_vtl_sidecar_run(self.node.state.file.as_raw_fd(), self.cpu)
                .map_err(|err| SidecarError::Io(err.into()))?;
        }
        Ok(())
    }

    fn start_async(&mut self) -> Result<(), SidecarError> {
        let old = std::mem::replace(
            &mut *self.node.state.vps[self.index].lock(),
            VpState::Running(None),
        );
        assert!(matches!(old, VpState::Stopped));
        // SAFETY: no safety requirements on this ioctl.
        unsafe {
            ioctl::mshv_vtl_sidecar_start(self.node.state.file.as_raw_fd(), self.cpu)
                .map_err(|err| SidecarError::Io(err.into()))?;
        }
        Ok(())
    }

    fn stop_async(&mut self) {
        // SAFETY: no safety requirements on this ioctl.
        unsafe {
            ioctl::mshv_vtl_sidecar_stop(self.node.state.file.as_raw_fd(), self.cpu)
                .expect("failed to stop vp");
        }
    }

    async fn wait_async(&mut self) {
        poll_fn(|cx| {
            let mut vp = self.node.state.vps[self.index].lock();
            match &mut *vp {
                VpState::Stopped => unreachable!(),
                VpState::Running(waker) => {
                    if waker.as_ref().is_none_or(|w| !cx.waker().will_wake(w)) {
                        *waker = Some(cx.waker().clone());
                    }
                    Poll::Pending
                }
                VpState::Finished => {
                    *vp = VpState::Stopped;
                    Poll::Ready(())
                }
            }
        })
        .await
    }

    fn command_result<
        O: FromBytes + Immutable + KnownLayout,
        S: FromBytes + Immutable + KnownLayout,
    >(
        &mut self,
        n: usize,
    ) -> Result<(&O, &[S]), SidecarError> {
        // SAFETY: the sidecar kernel will not concurrently modify the state
        // page after the command has completed.
        let shmem = unsafe { self.shmem.as_ref() };
        if shmem.command_page.has_error != 0 {
            let s = String::from_utf8_lossy(
                &shmem.command_page.error.buf[..shmem.command_page.error.len as usize],
            );
            return Err(SidecarError::Sidecar(s.into_owned()));
        }
        let (output, slice) = shmem
            .command_page
            .request_data
            .as_bytes()
            .split_at(size_of::<O>());
        let output = O::ref_from_bytes(output).unwrap();
        let (slice, _) = <[S]>::ref_from_prefix_with_elems(slice, n).unwrap();
        Ok((output, slice))
    }
}

/// An object representing a running VP.
///
/// Panics if dropped without waiting for the VP to stop.
pub struct SidecarRun<'a, 'b> {
    vp: &'a mut SidecarVp<'b>,
    waited: bool,
}

impl SidecarRun<'_, '_> {
    /// Requests that the sidecar kernel stop the VP.
    ///
    /// You must still call `wait` after this to ensure the VP has stopped.
    pub fn cancel(&mut self) {
        if !self.waited {
            self.vp.stop_async();
        }
    }

    /// Waits for the VP to stop.
    ///
    /// Returns `true` if the VP hit an intercept.
    pub async fn wait(&mut self) -> Result<bool, SidecarError> {
        if !self.waited {
            self.vp.wait_async().await;
            self.waited = true;
        }
        let &RunVpResponse { intercept } = self.vp.command_result::<_, u8>(0)?.0;
        Ok(intercept != 0)
    }
}

impl Drop for SidecarRun<'_, '_> {
    fn drop(&mut self) {
        assert!(self.waited, "failed to stop vp");
    }
}
