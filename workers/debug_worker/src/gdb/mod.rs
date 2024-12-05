// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use futures::executor::block_on;
use gdbstub::common::Tid;
use mesh::rpc::RpcSend;
use std::num::NonZeroUsize;
use vmm_core_defs::debug_rpc::DebugRequest;
use vmm_core_defs::debug_rpc::DebugStopReason;
use vmm_core_defs::debug_rpc::GuestAddress;
use vmm_core_defs::debug_rpc::HardwareBreakpoint;

pub mod arch;
pub mod targets;

#[derive(Debug, Default, Clone)]
pub struct Vp {
    pub single_step: bool,
}

#[derive(Debug)]
pub struct VmProxy {
    req_chan: mesh::Sender<DebugRequest>,
    stop_chan: Option<mesh::OneshotReceiver<DebugStopReason>>,

    pub vps: Box<[Vp]>,
    pub breakpoints: [Option<HardwareBreakpoint>; 4],
}

impl VmProxy {
    pub fn new(req_chan: mesh::Sender<DebugRequest>, vp_count: u32) -> Self {
        Self {
            req_chan,
            vps: vec![Vp::default(); vp_count as usize].into(),
            stop_chan: None,
            breakpoints: [None; 4],
        }
    }

    pub fn into_params(self) -> (mesh::Sender<DebugRequest>, u32) {
        (self.req_chan, self.vps.len() as u32)
    }

    pub fn send_req(&mut self, req: DebugRequest) {
        self.req_chan.send(req);
    }

    pub fn take_stop_chan(&mut self) -> Option<mesh::OneshotReceiver<DebugStopReason>> {
        self.stop_chan.take()
    }

    pub fn tid_to_vp(&self, tid: Tid) -> anyhow::Result<u32> {
        let index = tid.get() - 1;
        if index >= self.vps.len() {
            Err(anyhow::anyhow!("Tid {} doesn't correspond to a vp", tid))
        } else {
            Ok(index as u32)
        }
    }

    pub fn vp_to_tid(&self, vp: u32) -> Tid {
        NonZeroUsize::new(vp as usize + 1).unwrap()
    }

    #[allow(dead_code)] // TODO: add monitor command to inspect physical memory?
    fn read_guest_physical_memory(&mut self, gpa: u64, data: &mut [u8]) -> anyhow::Result<()> {
        let buf = block_on(self.req_chan.call_failable(
            DebugRequest::ReadMemory,
            (GuestAddress::Gpa(gpa), data.len()),
        ))
        .context("failed to read memory")?;
        data.copy_from_slice(
            buf.get(..data.len())
                .context("invalid return buffer length")?,
        );
        Ok(())
    }

    /// Reads `len` bytes from guest VP `vp_index`'s virtual address `gva`.
    fn read_guest_virtual_memory(
        &mut self,
        vp_index: u32,
        gva: u64,
        data: &mut [u8],
    ) -> anyhow::Result<()> {
        let buf = block_on(self.req_chan.call_failable(
            DebugRequest::ReadMemory,
            (GuestAddress::Gva { vp: vp_index, gva }, data.len()),
        ))
        .context("failed to read memory")?;
        data.copy_from_slice(
            buf.get(..data.len())
                .context("invalid return buffer length")?,
        );
        Ok(())
    }

    /// Writes `data` to guest VP `vp_index`'s virtual address `gva`.
    fn write_guest_virtual_memory(
        &mut self,
        vp_index: u32,
        gva: u64,
        data: &[u8],
    ) -> anyhow::Result<()> {
        block_on(self.req_chan.call_failable(
            DebugRequest::WriteMemory,
            (GuestAddress::Gva { vp: vp_index, gva }, data.to_vec()),
        ))
        .context("failed to write memory")?;
        Ok(())
    }
}
