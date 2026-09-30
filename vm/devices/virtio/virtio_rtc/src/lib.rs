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
use zerocopy::IntoBytes;

const RESPONSE_HEAD_SIZE: usize = size_of::<spec::RespHead>();
const MAX_REQUEST_SIZE: usize = size_of::<spec::ReqSetAlarm>();
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
                    let bytes_written = process_request(&state.mem, &work);
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

fn process_request(mem: &GuestMemory, work: &VirtioQueueCallbackWork) -> u32 {
    let readable_len = work.get_payload_length(false);
    let writable_len = work.get_payload_length(true);

    if writable_len == 0 {
        return 0;
    }
    let error_head_size = writable_len.min(RESPONSE_HEAD_SIZE as u64) as usize;
    if readable_len < size_of::<spec::ReqHead>() as u64 {
        return write_error_response(mem, work, spec::S_EINVAL, error_head_size);
    }

    let head_size = size_of::<spec::ReqHead>();
    let mut request = [0; MAX_REQUEST_SIZE];
    if let Err(status) = read_request_part(mem, work, 0, &mut request[..head_size]) {
        return write_error_response(mem, work, status, error_head_size);
    }

    let head =
        spec::ReqHead::read_from_bytes(&request[..head_size]).expect("fixed request header size");
    let msg_type = head.msg_type.get();
    let Some((request_size, response_size)) = request_layout(msg_type) else {
        return write_error_response(mem, work, spec::S_EOPNOTSUPP, error_head_size);
    };

    if writable_len < response_size as u64 {
        return write_error_response(mem, work, spec::S_EINVAL, error_head_size);
    }
    if readable_len < request_size as u64 {
        return write_error_response(mem, work, spec::S_EINVAL, response_size);
    }

    if request_size > head_size {
        if let Err(status) = read_request_part(
            mem,
            work,
            head_size as u64,
            &mut request[head_size..request_size],
        ) {
            return write_error_response(mem, work, status, response_size);
        }
    }

    handle_request(mem, work, head, &request[..request_size], response_size)
}

fn handle_request(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    head: spec::ReqHead,
    request: &[u8],
    response_size: usize,
) -> u32 {
    let msg_type = head.msg_type.get();
    if matches!(
        msg_type,
        spec::REQ_READ_ALARM | spec::REQ_SET_ALARM | spec::REQ_SET_ALARM_ENABLED
    ) {
        return write_error_response(mem, work, spec::S_ENODEV, response_size);
    }
    match msg_type {
        spec::REQ_CFG => {
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
            let request =
                spec::ReqClock::read_from_bytes(request).expect("validated clock request size");
            if request.clock_id.get() != 0 {
                return write_error_response(mem, work, spec::S_ENODEV, response_size);
            }
            if head.reserved != [0; 6] || request.reserved != [0; 6] {
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
            let request =
                spec::ReqClock::read_from_bytes(request).expect("validated clock request size");
            let reading = match request.clock_id.get() {
                0 => {
                    if head.reserved != [0; 6] || request.reserved != [0; 6] {
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
            let request =
                spec::ReqCross::read_from_bytes(request).expect("validated cross request size");
            if request.clock_id.get() != 0 {
                return write_error_response(mem, work, spec::S_ENODEV, response_size);
            }
            match request.hw_counter {
                spec::COUNTER_ARM_VCT | spec::COUNTER_X86_TSC => {}
                spec::COUNTER_INVALID => {
                    return write_error_response(mem, work, spec::S_EINVAL, response_size);
                }
                _ => {
                    return write_error_response(mem, work, spec::S_EOPNOTSUPP, response_size);
                }
            }
            if head.reserved != [0; 6] || request.reserved != [0; 5] {
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
        _ => unreachable!("known request layout must have a handler"),
    }
}

fn write_error_response(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    status: u8,
    len: usize,
) -> u32 {
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

fn read_request_part(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    offset: u64,
    bytes: &mut [u8],
) -> Result<(), u8> {
    match work.read_at_offset(offset, mem, bytes) {
        Ok(bytes_read) if bytes_read == bytes.len() => Ok(()),
        Ok(_) => Err(spec::S_EINVAL),
        Err(err) => {
            tracelimit::error_ratelimited!(
                err = &err as &dyn std::error::Error,
                "failed to read virtio-rtc request"
            );
            Err(spec::S_EIO)
        }
    }
}

fn request_layout(msg_type: u16) -> Option<(usize, usize)> {
    match msg_type {
        spec::REQ_READ => Some((size_of::<spec::ReqClock>(), size_of::<spec::RespRead>())),
        spec::REQ_READ_CROSS => Some((
            size_of::<spec::ReqCross>(),
            size_of::<spec::RespReadCross>(),
        )),
        spec::REQ_CFG => Some((size_of::<spec::ReqHead>(), size_of::<spec::RespCfg>())),
        spec::REQ_CLOCK_CAP => Some((size_of::<spec::ReqClock>(), size_of::<spec::RespClockCap>())),
        spec::REQ_CROSS_CAP => Some((size_of::<spec::ReqCross>(), size_of::<spec::RespCrossCap>())),
        spec::REQ_READ_ALARM => Some((
            size_of::<spec::ReqClock>(),
            size_of::<spec::RespReadAlarm>(),
        )),
        spec::REQ_SET_ALARM => Some((size_of::<spec::ReqSetAlarm>(), size_of::<spec::RespHead>())),
        spec::REQ_SET_ALARM_ENABLED => Some((
            size_of::<spec::ReqSetAlarmEnabled>(),
            size_of::<spec::RespHead>(),
        )),
        _ => None,
    }
}

fn ok_head() -> spec::RespHead {
    spec::RespHead {
        status: spec::S_OK,
        reserved: [0; 7],
    }
}
