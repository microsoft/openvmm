# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Sends an Add SEL command through Linux's /dev/ipmi ioctl interface. Sysfs can
# verify device discovery but cannot submit the command, and ipmitool is not
# installed in the Petri Ubuntu image. This can be replaced by ipmitool if it
# becomes a guaranteed image dependency, or by a dedicated Rust guest utility.

import ctypes
import os
import select

IPMI_SYSTEM_INTERFACE_ADDR_TYPE = 0x0C
IPMI_BMC_CHANNEL = 0x0F
IPMI_RESPONSE_RECV_TYPE = 1
IOC_WRITE = 1
IOC_READ = 2


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
    if result == -1:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


device_path = next(
    (
        path
        for path in ("/dev/ipmi0", "/dev/ipmi/0", "/dev/ipmidev/0")
        if os.path.exists(path)
    ),
    None,
)
if device_path is None:
    raise RuntimeError("Linux IPMI device was not created")

libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
libc.ioctl.restype = ctypes.c_int

address = IpmiSystemInterfaceAddr(IPMI_SYSTEM_INTERFACE_ADDR_TYPE, IPMI_BMC_CHANNEL, 0)
request_data = (ctypes.c_ubyte * 16)(
    0x00,
    0x00,
    0x02,
    0x00,
    0x00,
    0x00,
    0x00,
    0x20,
    0x00,
    0x04,
    0x09,
    0x01,
    0x6F,
    0xDE,
    0xAD,
    0xBE,
)
request = IpmiReq(
    ctypes.addressof(address),
    ctypes.sizeof(address),
    1,
    IpmiMsg(0x0A, 0x44, len(request_data), ctypes.addressof(request_data)),
)

fd = os.open(device_path, os.O_RDWR)
try:
    # Linux defines IPMICTL_SEND_COMMAND with _IOR despite the command sending
    # request data from userspace to the kernel.
    ioctl(fd, ioctl_code(IOC_READ, 13, ctypes.sizeof(IpmiReq)), request)
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
    ioctl(fd, ioctl_code(IOC_READ | IOC_WRITE, 11, ctypes.sizeof(IpmiRecv)), response)

    data = bytes(response_data[: response.msg.data_len])
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
