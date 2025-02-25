// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::vmsif;
use guid::Guid;
use std::ffi::OsStr;
use std::io;
use std::os::windows::prelude::*;
use std::ptr::null;
use std::ptr::null_mut;
use std::time::Duration;
use widestring::U16CString;

pub struct KernelVmNic {
    nic: OwnedHandle,
    nic_name: String,
}

pub(crate) fn c16(s: impl AsRef<OsStr>) -> io::Result<U16CString> {
    U16CString::from_os_str(s.as_ref())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul character in string"))
}

impl KernelVmNic {
    pub fn new(
        vm_id: &Guid,
        nic_name: &str,
        friendly_name: &str,
        mac_address: [u8; 6],
        instance_id: &Guid,
        vmbus_handle: BorrowedHandle<'_>,
    ) -> io::Result<Self> {
        let full_nic_name = format!("{}--{}", vm_id, nic_name);
        let path = format!(r#"\\.\VmSwitch\{}"#, &full_nic_name);

        // BUGBUG: Generate a random partition ID since the switch relies on
        // this being unique or the machine will bugcheck. Fix the switch.
        let mut partition_id = [0; 8];
        getrandom::getrandom(&mut partition_id).expect("rng failure");

        let handle = unsafe {
            let mut raw_handle = null_mut();
            vmsif::chk(vmsif::VmsIfNicCreateSynthetic(
                &mut raw_handle,
                c16(path)?.as_ptr(),
            ))?;
            let handle = OwnedHandle::from_raw_handle(raw_handle);
            let vm_id_16 = c16(vm_id.to_string())?;
            vmsif::chk(vmsif::VmsIfNicMorphToSynthNic(
                handle.as_raw_handle(),
                c16(&full_nic_name)?.as_ptr(),
                c16(friendly_name)?.as_ptr(),
                vm_id_16.as_ptr(),
                vm_id_16.as_ptr(),
                &mac_address,
                true,
                u64::from_ne_bytes(partition_id),
                vmbus_handle.as_raw_handle(),
                false,
                *instance_id,
                0x100,
                false,
                false,
                false,
                0,
                false,
                false,
                0,
            ))?;

            handle
        };

        Ok(Self {
            nic: handle,
            nic_name: full_nic_name,
        })
    }

    /// Connects the NIC to a port on the given switch.
    pub fn connect(&mut self, id: &SwitchPortId) -> io::Result<()> {
        let (switch16, port16) = id.c_ids();
        unsafe {
            vmsif::chk(vmsif::VmsIfNicConnect(
                self.nic.as_raw_handle(),
                switch16.as_ptr(),
                port16.as_ptr(),
                c16(&self.nic_name)?.as_ptr(),
                Duration::from_secs(10).as_millis() as u32,
            ))?;

            Ok(())
        }
    }

    pub fn suspend(&self) -> io::Result<()> {
        unsafe {
            vmsif::chk(vmsif::VmsIfNicSuspendSynthetic(
                self.nic.as_raw_handle(),
                [0].as_ptr(),
            ))
        }
    }

    pub fn resume(&self) -> io::Result<()> {
        unsafe {
            vmsif::chk(vmsif::VmsIfNicResumeSynthetic(
                self.nic.as_raw_handle(),
                [0].as_ptr(),
                0,
                null_mut(),
            ))
        }
    }
}

#[derive(Debug)]
// Just a newtype to give a better name and ergonomics, field needs to be held to keep handle alive.
pub struct SwitchPort(#[allow(dead_code)] OwnedHandle);

impl SwitchPort {
    pub fn new(id: &SwitchPortId) -> io::Result<Self> {
        let mut raw_handle = null_mut();
        let (switch16, port16) = id.c_ids();

        // SAFETY: no special considerations to call this API
        unsafe {
            vmsif::chk(vmsif::VmsIfPortCreateWithHandle(
                &mut raw_handle,
                switch16.as_ptr(),
                port16.as_ptr(),
                null(),
                4, /* emulated */
                0,
                0,
                0x1173, // lite
            ))?;

            Ok(Self(OwnedHandle::from_raw_handle(raw_handle)))
        }
    }
}

#[derive(Clone, Debug)]
pub struct SwitchPortId {
    pub switch: Guid,
    pub port: Guid,
}

impl SwitchPortId {
    pub(crate) fn c_ids(&self) -> (U16CString, U16CString) {
        (
            c16(self.switch.to_string()).expect("always valid string"),
            c16(self.port.to_string()).expect("always valid string"),
        )
    }
}
