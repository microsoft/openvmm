// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the OpenHCL IPMI KCS interface.

use anyhow::Context;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use vmm_test_macros::openvmm_test;

const LINUX_IPMI_TEST: &str = r#"
import ctypes
import os
import select

IPMI_SYSTEM_INTERFACE_ADDR_TYPE = 0x0c
IPMI_BMC_CHANNEL = 0x0f
IPMI_RESPONSE_RECV_TYPE = 1

class IpmiSystemInterfaceAddr(ctypes.Structure):
    _fields_ = [
        ("addr_type", ctypes.c_int),
        ("channel", ctypes.c_short),
        ("lun", ctypes.c_ubyte),
    ]

class IpmiMsg(ctypes.Structure):
    _fields_ = [
        ("netfn", ctypes.c_ubyte),
        ("cmd", ctypes.c_ubyte),
        ("data_len", ctypes.c_ushort),
        ("data", ctypes.c_void_p),
    ]

class IpmiReq(ctypes.Structure):
    _fields_ = [
        ("addr", ctypes.c_void_p),
        ("addr_len", ctypes.c_uint),
        ("msgid", ctypes.c_long),
        ("msg", IpmiMsg),
    ]

class IpmiRecv(ctypes.Structure):
    _fields_ = [
        ("recv_type", ctypes.c_int),
        ("addr", ctypes.c_void_p),
        ("addr_len", ctypes.c_uint),
        ("msgid", ctypes.c_long),
        ("msg", IpmiMsg),
    ]

def ioctl_code(direction, number, size):
    return (direction << 30) | (size << 16) | (ord("i") << 8) | number

def ioctl(fd, request, value):
    result = libc.ioctl(fd, request, ctypes.byref(value))
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))

device_path = next(
    (path for path in ("/dev/ipmi0", "/dev/ipmi/0", "/dev/ipmidev/0") if os.path.exists(path)),
    None,
)
if device_path is None:
    raise RuntimeError("Linux IPMI device was not created")

libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
libc.ioctl.restype = ctypes.c_int

address = IpmiSystemInterfaceAddr(IPMI_SYSTEM_INTERFACE_ADDR_TYPE, IPMI_BMC_CHANNEL, 0)
request_data = (ctypes.c_ubyte * 16)(
    0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x20,
    0x00, 0x04, 0x09, 0x01, 0x6f, 0xde, 0xad, 0xbe,
)
request = IpmiReq(
    ctypes.addressof(address),
    ctypes.sizeof(address),
    1,
    IpmiMsg(0x0a, 0x44, len(request_data), ctypes.addressof(request_data)),
)

fd = os.open(device_path, os.O_RDWR)
try:
    ioctl(fd, ioctl_code(2, 13, ctypes.sizeof(IpmiReq)), request)
    if not select.select([fd], [], [], 10)[0]:
        raise TimeoutError("timed out waiting for the Add SEL response")

    response_address = (ctypes.c_ubyte * 32)()
    response_data = (ctypes.c_ubyte * 64)()
    response = IpmiRecv(
        0,
        ctypes.addressof(response_address),
        len(response_address),
        0,
        IpmiMsg(0, 0, len(response_data), ctypes.addressof(response_data)),
    )
    ioctl(fd, ioctl_code(3, 11, ctypes.sizeof(IpmiRecv)), response)

    data = bytes(response_data[:response.msg.data_len])
    if response.recv_type != IPMI_RESPONSE_RECV_TYPE:
        raise RuntimeError(f"unexpected receive type {response.recv_type}")
    if response.msgid != 1 or response.msg.cmd != 0x44:
        raise RuntimeError(
            f"unexpected response msgid={response.msgid} command={response.msg.cmd:#x}"
        )
    if len(data) != 3 or data[0] != 0:
        raise RuntimeError(f"Add SEL failed: {data.hex()}")

    print(f"ADDSEL_CC=0 RECORD_ID={int.from_bytes(data[1:3], 'little')}")
finally:
    os.close(fd)
"#;

const WINDOWS_IPMI_TEST: &str = r#"
$ipmi = Get-CimInstance -Namespace root\wmi -ClassName Microsoft_IPMI -ErrorAction Stop
$record = [byte[]](
    0x00,0x00,0x02,0x00,0x00,0x00,0x00,0x20,
    0x00,0x04,0x09,0x01,0x6f,0xde,0xad,0xbe
)
$response = Invoke-CimMethod -InputObject $ipmi -MethodName RequestResponse -Arguments @{
    NetworkFunction  = [byte]0x0A
    Lun              = [byte]0x00
    ResponderAddress = [byte]0x20
    Command          = [byte]0x44
    RequestData      = $record
    RequestDataSize  = [uint32]$record.Length
} -ErrorAction Stop
if ($response.CompletionCode -ne 0) {
    throw "Add SEL failed with completion code $($response.CompletionCode)"
}
Write-Output "ADDSEL_CC=0"
"#;

#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
async fn ipmi_kcs_add_sel(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (mut vm, agent) = config.with_ipmi(true).run().await?;

    let output = match os_flavor {
        OsFlavor::Linux => {
            let shell = agent.unix_shell();
            cmd!(shell, "sudo modprobe ipmi_si").run().await?;
            cmd!(shell, "sudo modprobe ipmi_devintf").run().await?;
            agent
                .write_file("/tmp/ipmi_test.py", LINUX_IPMI_TEST.as_bytes())
                .await
                .context("failed to copy the Linux IPMI test into the guest")?;
            cmd!(shell, "sudo python3 /tmp/ipmi_test.py").read().await?
        }
        OsFlavor::Windows => {
            let shell = agent.windows_shell();
            cmd!(shell, "powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    WINDOWS_IPMI_TEST,
                ])
                .read()
                .await?
        }
        _ => unreachable!(),
    };

    anyhow::ensure!(
        output.contains("ADDSEL_CC=0"),
        "guest did not successfully add an IPMI SEL record: {output}"
    );

    let notification = loop {
        let notification = vm.backend().wait_for_ipmi_sel().await?;
        if notification.record[2] == 0x02
            && notification.record[7..] == [0x20, 0x00, 0x04, 0x09, 0x01, 0x6f, 0xde, 0xad, 0xbe]
        {
            break notification;
        }

        tracing::info!(?notification, "ignoring an unrelated IPMI SEL notification");
    };
    anyhow::ensure!(
        notification.record_id != 0
            && notification.record[0..2] == notification.record_id.to_le_bytes(),
        "host received an invalid IPMI SEL record ID: {notification:?}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
