// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// C ABI for the shared IPMI KCS BMC core (ipmi_kcs_ffi staticlib).
//
// Link against ipmi_kcs_ffi.lib. The host drives the device by forwarding
// guest I/O-port accesses on the KCS data/status ports to ipmi_kcs_io_read /
// ipmi_kcs_io_write, and supplies a SEL callback (to forward committed SEL
// entries, e.g. to ETW) and a clock callback.

#ifndef IPMI_KCS_FFI_H
#define IPMI_KCS_FFI_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C"
{
#endif

// Opaque device handle.
typedef struct IpmiKcsDevice IpmiKcsDevice;

// Return codes.
#define IPMI_KCS_OK 0
#define IPMI_KCS_NULL (-1)
#define IPMI_KCS_INVALID_REGISTER (-2)

// Size of a SEL record passed to the SEL callback.
#define IPMI_KCS_SEL_RECORD_SIZE 16

// Called after a SEL entry is committed. `record` points to `record_len` bytes
// (always IPMI_KCS_SEL_RECORD_SIZE), valid only for the duration of the call.
// `ctx` is the opaque pointer supplied to ipmi_kcs_new.
typedef void (*IpmiKcsSelCallback)(
    void* ctx,
    uint16_t record_id,
    const uint8_t* record,
    size_t record_len);

// Returns the current wall-clock time in seconds since the Unix epoch.
typedef int64_t (*IpmiKcsClockCallback)(void* ctx);

// Create a new device. Either callback may be NULL. The returned handle must be
// released with ipmi_kcs_free. The callback contexts must outlive the device.
IpmiKcsDevice* ipmi_kcs_new(
    IpmiKcsSelCallback sel_cb,
    void* sel_ctx,
    IpmiKcsClockCallback clock_cb,
    void* clock_ctx);

// Free a device. Passing NULL is a no-op.
void ipmi_kcs_free(IpmiKcsDevice* dev);

// Read a KCS register into *out_byte.
// Returns IPMI_KCS_OK, IPMI_KCS_NULL, or IPMI_KCS_INVALID_REGISTER.
int32_t ipmi_kcs_io_read(IpmiKcsDevice* dev, uint16_t port, uint8_t* out_byte);

// Write a byte to a KCS register.
// Returns IPMI_KCS_OK, IPMI_KCS_NULL, or IPMI_KCS_INVALID_REGISTER.
int32_t ipmi_kcs_io_write(IpmiKcsDevice* dev, uint16_t port, uint8_t byte);

// Reset the device to IDLE and clear the SEL.
void ipmi_kcs_reset(IpmiKcsDevice* dev);

// The KCS data register I/O port (0xCA2).
uint16_t ipmi_kcs_data_port(void);

// The KCS status/command register I/O port (0xCA3).
uint16_t ipmi_kcs_status_port(void);

#ifdef __cplusplus
}
#endif

#endif // IPMI_KCS_FFI_H
