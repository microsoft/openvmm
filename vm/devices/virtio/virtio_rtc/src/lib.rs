// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio RTC device implementation.
//!
//! Implements the virtio-rtc device (device ID 17) as specified in the
//! VIRTIO 1.4 specification, §5.23 "RTC Device". Currently implements
//! a single UTC clock.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;
mod spec;

#[cfg(test)]
mod tests;

use anyhow::Context as _;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const RESPONSE_HEAD_SIZE: usize = size_of::<spec::RespHead>();
const MAX_RESPONSE_SIZE: usize = size_of::<spec::RespReadCross>();

#[derive(InspectMut)]
pub struct VirtioRtcDevice {
    driver: VmTaskDriver,
    #[inspect(mut)]
    worker: TaskControl<RtcWorker, RtcQueue>,
}

impl VirtioRtcDevice {
    pub fn new(driver_source: &VmTaskDriverSource) -> Self {
        Self {
            driver: driver_source.simple(),
            worker: TaskControl::new(RtcWorker),
        }
    }
}

impl VirtioDevice for VirtioRtcDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::RTC,
            device_features: VirtioDeviceFeatures::new()
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1,
            device_register_length: 0,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        assert_eq!(idx, 0);

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create polled wait")?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.worker.insert(
            self.driver.clone(),
            "virtio-rtc-queue",
            RtcQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        assert_eq!(idx, 0);
        if !self.worker.has_state() {
            return None;
        }
        self.worker.stop().await;
        Some(self.worker.remove().queue.queue_state())
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

#[derive(InspectMut)]
struct RtcWorker;

#[derive(InspectMut)]
struct RtcQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl InspectTaskMut<RtcQueue> for RtcWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut RtcQueue>) {
        req.respond().merge(self).merge(state);
    }
}

impl AsyncRun<RtcQueue> for RtcWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut RtcQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    let bytes_written = handle_request(&state.mem, &work);
                    state.queue.complete(work, bytes_written);
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        "virtio-rtc queue error"
                    );
                    break;
                }
            }
        }
        Ok(())
    }
}

fn handle_request(mem: &GuestMemory, work: &VirtioQueueCallbackWork) -> u32 {
    if work.get_payload_length(true) == 0 {
        return 0;
    }
    if work.get_payload_length(false) < size_of::<spec::ReqHead>() as u64 {
        return write_error_response(mem, work, spec::S_EINVAL, RESPONSE_HEAD_SIZE);
    }

    let mut head = spec::ReqHead::new_zeroed();
    match work.read(mem, head.as_mut_bytes()) {
        Ok(bytes_read) if bytes_read == size_of::<spec::ReqHead>() => {}
        Ok(_) => return write_error_response(mem, work, spec::S_EINVAL, RESPONSE_HEAD_SIZE),
        Err(err) => {
            tracelimit::error_ratelimited!(
                err = &err as &dyn std::error::Error,
                "failed to read virtio-rtc request"
            );
            return write_error_response(mem, work, spec::S_EIO, RESPONSE_HEAD_SIZE);
        }
    }

    let msg_type = head.msg_type.get();
    match msg_type {
        spec::REQ_CFG => {
            let response_size = size_of::<spec::RespCfg>();
            if work.get_payload_length(true) < response_size as u64 {
                return write_error_response(mem, work, spec::S_EINVAL, response_size);
            }
            if head.reserved != [0; 6] {
                return write_error_response(mem, work, spec::S_EINVAL, response_size);
            }
            let response = spec::RespCfg {
                head: ok_head(),
                num_clocks: virtio::spec::u16_le::new(1),
                reserved: [0; 6],
            };
            write_response(mem, work, response.as_bytes())
        }
        spec::REQ_CLOCK_CAP => {
            let response_size = size_of::<spec::RespClockCap>();
            let body = match read_request_body::<spec::ReqClockBody>(mem, work, response_size) {
                Ok(body) => body,
                Err(status) => return write_error_response(mem, work, status, response_size),
            };
            if body.clock_id.get() != 0 {
                return write_error_response(mem, work, spec::S_ENODEV, response_size);
            }
            if head.reserved != [0; 6] || body.reserved != [0; 6] {
                return write_error_response(mem, work, spec::S_EINVAL, response_size);
            }
            let response = spec::RespClockCap {
                head: ok_head(),
                clock_type: spec::CLOCK_UTC_MAYBE_SMEARED,
                leap_second_smearing: spec::SMEAR_UNSPECIFIED,
                flags: 0,
                reserved: [0; 5],
            };
            write_response(mem, work, response.as_bytes())
        }
        spec::REQ_READ => {
            let response_size = size_of::<spec::RespRead>();
            let body = match read_request_body::<spec::ReqClockBody>(mem, work, response_size) {
                Ok(body) => body,
                Err(status) => return write_error_response(mem, work, status, response_size),
            };
            let reading = match body.clock_id.get() {
                0 => {
                    if head.reserved != [0; 6] || body.reserved != [0; 6] {
                        return write_error_response(mem, work, spec::S_EINVAL, response_size);
                    }
                    clock_reading_ns(SystemTime::now())
                }
                _ => return write_error_response(mem, work, spec::S_ENODEV, response_size),
            };
            match reading {
                Ok(clock_reading) => {
                    let response = spec::RespRead {
                        head: ok_head(),
                        clock_reading: virtio::spec::u64_le::new(clock_reading),
                    };
                    write_response(mem, work, response.as_bytes())
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        err = err.as_ref() as &dyn std::error::Error,
                        "failed to read virtio-rtc clock"
                    );
                    write_error_response(mem, work, spec::S_EIO, response_size)
                }
            }
        }
        spec::REQ_CROSS_CAP | spec::REQ_READ_CROSS => {
            let response_size = if msg_type == spec::REQ_CROSS_CAP {
                size_of::<spec::RespCrossCap>()
            } else {
                size_of::<spec::RespReadCross>()
            };
            let body = match read_request_body::<spec::ReqCrossBody>(mem, work, response_size) {
                Ok(body) => body,
                Err(status) => return write_error_response(mem, work, status, response_size),
            };
            if body.clock_id.get() != 0 {
                return write_error_response(mem, work, spec::S_ENODEV, response_size);
            }
            match body.hw_counter {
                spec::COUNTER_ARM_VCT | spec::COUNTER_X86_TSC => {}
                spec::COUNTER_INVALID => {
                    return write_error_response(mem, work, spec::S_EINVAL, response_size);
                }
                _ => {
                    return write_error_response(mem, work, spec::S_EOPNOTSUPP, response_size);
                }
            }
            if head.reserved != [0; 6] || body.reserved != [0; 5] {
                return write_error_response(mem, work, spec::S_EINVAL, response_size);
            }
            if msg_type == spec::REQ_READ_CROSS {
                return write_error_response(mem, work, spec::S_EOPNOTSUPP, response_size);
            }
            let response = spec::RespCrossCap {
                head: ok_head(),
                flags: 0,
                reserved: [0; 7],
            };
            write_response(mem, work, response.as_bytes())
        }
        spec::REQ_READ_ALARM => {
            let response_size = size_of::<spec::RespReadAlarm>();
            let status = match read_request_body::<spec::ReqClockBody>(mem, work, response_size) {
                Ok(_) => spec::S_ENODEV,
                Err(status) => status,
            };
            write_error_response(mem, work, status, response_size)
        }
        spec::REQ_SET_ALARM => {
            let status =
                match read_request_body::<spec::ReqSetAlarmBody>(mem, work, RESPONSE_HEAD_SIZE) {
                    Ok(_) => spec::S_ENODEV,
                    Err(status) => status,
                };
            write_error_response(mem, work, status, RESPONSE_HEAD_SIZE)
        }
        spec::REQ_SET_ALARM_ENABLED => {
            let status = match read_request_body::<spec::ReqSetAlarmEnabledBody>(
                mem,
                work,
                RESPONSE_HEAD_SIZE,
            ) {
                Ok(_) => spec::S_ENODEV,
                Err(status) => status,
            };
            write_error_response(mem, work, status, RESPONSE_HEAD_SIZE)
        }
        _ => write_error_response(mem, work, spec::S_EOPNOTSUPP, RESPONSE_HEAD_SIZE),
    }
}

fn write_error_response(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    status: u8,
    len: usize,
) -> u32 {
    let writable_len = work.get_payload_length(true);
    let len = if writable_len < len as u64 {
        writable_len.min(RESPONSE_HEAD_SIZE as u64) as usize
    } else {
        len
    };
    if len == 0 {
        return 0;
    }
    let mut bytes = [0; MAX_RESPONSE_SIZE];
    bytes[0] = status;
    write_response(mem, work, &bytes[..len])
}

fn write_response(mem: &GuestMemory, work: &VirtioQueueCallbackWork, bytes: &[u8]) -> u32 {
    match work.write(mem, bytes) {
        Ok(()) => bytes.len() as u32,
        Err(err) => {
            tracelimit::error_ratelimited!(
                err = &err as &dyn std::error::Error,
                "failed to write virtio-rtc response"
            );
            0
        }
    }
}

fn clock_reading_ns(time: SystemTime) -> anyhow::Result<u64> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .context("host clock is before the Unix epoch")?;
    u64::try_from(duration.as_nanos()).context("host clock exceeds the virtio RTC range")
}

fn read_request_body<Body: FromBytes + IntoBytes>(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    response_size: usize,
) -> Result<Body, u8> {
    if work.get_payload_length(true) < response_size as u64 {
        return Err(spec::S_EINVAL);
    }
    let header_size = size_of::<spec::ReqHead>();
    let request_size = header_size + size_of::<Body>();
    if work.get_payload_length(false) < request_size as u64 {
        return Err(spec::S_EINVAL);
    }

    let mut body = Body::new_zeroed();
    match work.read_at_offset(header_size as u64, mem, body.as_mut_bytes()) {
        Ok(bytes_read) if bytes_read == size_of::<Body>() => Ok(body),
        Ok(_) => Err(spec::S_EINVAL),
        Err(err) => {
            tracelimit::error_ratelimited!(
                err = &err as &dyn std::error::Error,
                "failed to read virtio-rtc request body"
            );
            Err(spec::S_EIO)
        }
    }
}

fn ok_head() -> spec::RespHead {
    spec::RespHead {
        status: spec::S_OK,
        reserved: [0; 7],
    }
}
