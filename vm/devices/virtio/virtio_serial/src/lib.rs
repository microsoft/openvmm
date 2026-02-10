// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use guestmem::GuestMemory;
use parking_lot::Condvar;
use parking_lot::Mutex;
use std::io;
use std::io::ErrorKind;
use std::ops::DerefMut;
use std::sync::Arc;
use std::thread::JoinHandle;
use virtio::DeviceTraits;
use virtio::LegacyVirtioDevice;
use virtio::VirtioQueueCallbackWork;
use virtio::VirtioQueueWorkerContext;
use virtio::VirtioState;
use virtio::spec::VirtioDeviceFeatures;

const VIRTIO_DEVICE_TYPE_CONSOLE: u16 = 3;

// const VIRTIO_CONSOLE_F_SIZE: u32 = 1;
const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 2;
// const VIRTIO_CONSOLE_F_EMERG_WRITE: u32 = 4;

const VIRTIO_CONSOLE_DEVICE_READY: u16 = 0;
const VIRTIO_CONSOLE_DEVICE_ADD: u16 = 1;
// const VIRTIO_CONSOLE_DEVICE_REMOVE: u16 = 2;
const VIRTIO_CONSOLE_PORT_READY: u16 = 3;
// const VIRTIO_CONSOLE_CONSOLE_PORT: u16 = 4;
// const VIRTIO_CONSOLE_RESIZE: u16 = 5;
const VIRTIO_CONSOLE_PORT_OPEN: u16 = 6;
const VIRTIO_CONSOLE_PORT_NAME: u16 = 7;

enum VirtioSerialPortIoState {
    Unavailable,
    Ready(Option<VirtioQueueCallbackWork>),
    Processing,
    Disconnected,
    Exiting,
}

pub struct VirtioSerialPort {
    mem: GuestMemory,
    read_state: (Mutex<VirtioSerialPortIoState>, Condvar),
    write_state: (Mutex<VirtioSerialPortIoState>, Condvar),
}

impl VirtioSerialPort {
    pub fn new(mem: &GuestMemory) -> Self {
        Self {
            mem: mem.clone(),
            read_state: (
                Mutex::new(VirtioSerialPortIoState::Disconnected),
                Condvar::new(),
            ),
            write_state: (
                Mutex::new(VirtioSerialPortIoState::Disconnected),
                Condvar::new(),
            ),
        }
    }

    pub fn read_from_port(&self) -> Option<VirtioQueueCallbackWork> {
        let mut read_work: Option<VirtioQueueCallbackWork> = None;
        let (state, state_cvar) = &self.read_state;
        let mut cur_state = state.lock();
        while let VirtioSerialPortIoState::Unavailable = *cur_state {
            state_cvar.wait(&mut cur_state);
        }

        if let VirtioSerialPortIoState::Ready(work) = cur_state.deref_mut() {
            assert!(work.is_some());
            read_work = work.take();
            *cur_state = VirtioSerialPortIoState::Processing;
        }
        read_work
    }

    pub fn complete_read_from_port(&self, mut work: VirtioQueueCallbackWork) {
        work.complete(0);
        let (state, state_cvar) = &self.read_state;
        let mut cur_state = state.lock();
        if let VirtioSerialPortIoState::Processing = *cur_state {
            *cur_state = VirtioSerialPortIoState::Unavailable;
            state_cvar.notify_one();
        }
    }

    pub fn write_to_port(&self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut write_work: Option<VirtioQueueCallbackWork> = None;
        let (state, state_cvar) = &self.write_state;
        {
            let mut cur_state = state.lock();
            while let VirtioSerialPortIoState::Unavailable = *cur_state {
                state_cvar.wait(&mut cur_state);
            }

            if let VirtioSerialPortIoState::Ready(work) = cur_state.deref_mut() {
                assert!(work.is_some());
                write_work = work.take();
                *cur_state = VirtioSerialPortIoState::Processing;
            }
        }

        let mut bytes_written = 0;
        if let Some(mut work) = write_work {
            let mut remaining = data;
            if remaining.len() > u32::MAX as usize {
                remaining = &remaining[..u32::MAX as usize];
            }
            for payload in work.payload.iter() {
                if remaining.is_empty() {
                    break;
                }
                if !payload.writeable {
                    break;
                }
                if payload.length == 0 {
                    continue;
                }
                let bytes_to_write = std::cmp::min(payload.length as usize, remaining.len());
                if let Err(error) = self.mem.write_at(payload.address, remaining) {
                    tracing::error!(
                        error = &error as &dyn std::error::Error,
                        "[virtio_serial] Failed to write to guest memory",
                    );
                    break;
                }
                remaining = &remaining[bytes_to_write..];
                bytes_written += bytes_to_write;
            }
            work.complete(u32::try_from(bytes_written).unwrap());
            let mut cur_state = state.lock();
            if let VirtioSerialPortIoState::Processing = *cur_state {
                *cur_state = VirtioSerialPortIoState::Unavailable;
                state_cvar.notify_one();
            }
        }
        bytes_written
    }

    pub fn write_all_to_port(&self, data: &[u8]) -> bool {
        let mut remaining = data;
        while !remaining.is_empty() {
            let bytes_written = self.write_to_port(remaining);
            if bytes_written == 0 {
                break;
            }
            remaining = &remaining[bytes_written..];
        }
        remaining.is_empty()
    }

    // transfer data into the virtio port
    pub async fn process_virtio_read(&self, work: VirtioQueueCallbackWork) -> bool {
        let (state, state_cvar) = &self.write_state;
        let mut cur_state = state.lock();
        match *cur_state {
            VirtioSerialPortIoState::Unavailable | VirtioSerialPortIoState::Disconnected => {
                *cur_state = VirtioSerialPortIoState::Ready(Some(work));
                state_cvar.notify_one();
            }
            VirtioSerialPortIoState::Exiting => {
                return false;
            }
            _ => panic!("Unexpected serial port IO state"),
        };
        loop {
            match *cur_state {
                VirtioSerialPortIoState::Unavailable
                | VirtioSerialPortIoState::Disconnected
                | VirtioSerialPortIoState::Exiting => {
                    break false;
                }
                _ => {
                    state_cvar.wait(&mut cur_state);
                }
            };
        }
    }

    // transfer data from the virtio port
    pub async fn process_virtio_write(&self, mut work: VirtioQueueCallbackWork) -> bool {
        let (state, state_cvar) = &self.read_state;
        let mut cur_state = state.lock();
        match *cur_state {
            VirtioSerialPortIoState::Unavailable => {
                *cur_state = VirtioSerialPortIoState::Ready(Some(work));
                state_cvar.notify_one();
            }
            VirtioSerialPortIoState::Disconnected => {
                // if the port is disconnected, drop any writes
                work.complete(0);
                return true;
            }
            VirtioSerialPortIoState::Exiting => {
                return false;
            }
            _ => panic!("Unexpected serial port IO state"),
        };
        loop {
            match *cur_state {
                VirtioSerialPortIoState::Unavailable
                | VirtioSerialPortIoState::Disconnected
                | VirtioSerialPortIoState::Exiting => {
                    break false;
                }
                _ => {
                    state_cvar.wait(&mut cur_state);
                }
            };
        }
    }

    pub fn open(&self) {
        let (state, _) = &self.read_state;
        let mut cur_state = state.lock();
        if let VirtioSerialPortIoState::Disconnected = *cur_state {
            *cur_state = VirtioSerialPortIoState::Unavailable;
        } else {
            panic!("Opening a port that has already been opened");
        }

        let (state, _) = &self.write_state;
        let mut cur_state = state.lock();
        if let VirtioSerialPortIoState::Disconnected = *cur_state {
            *cur_state = VirtioSerialPortIoState::Unavailable;
        }
    }

    pub fn close(&self) {
        let (state, state_cvar) = &self.read_state;
        let mut cur_state = state.lock();
        if let VirtioSerialPortIoState::Ready(work) = cur_state.deref_mut() {
            assert!(work.is_some());
            let mut work = work.take().expect("[VIRTO SERIAL] empty work");
            work.complete(0);
        } else if let VirtioSerialPortIoState::Disconnected = *cur_state {
            panic!("Closing a port that was not open");
        }
        *cur_state = VirtioSerialPortIoState::Disconnected;
        state_cvar.notify_one();

        let (state, state_cvar) = &self.write_state;
        let mut cur_state = state.lock();
        *cur_state = VirtioSerialPortIoState::Disconnected;
        state_cvar.notify_one();
    }

    pub fn stop(&self) {
        let (state, state_cvar) = &self.read_state;
        *state.lock() = VirtioSerialPortIoState::Exiting;
        state_cvar.notify_one();
        let (state, state_cvar) = &self.write_state;
        *state.lock() = VirtioSerialPortIoState::Exiting;
        state_cvar.notify_one();
    }
}

impl Drop for VirtioSerialPort {
    fn drop(&mut self) {
        self.stop();
    }
}

struct VirtioSerialDeviceConfig {
    columns: u16,
    rows: u16,
    max_ports: u32,
    _emergency_write: u32,
}

struct VirtioSerialControl {
    port_number: u32,
    event: u16,
    value: u16,
}

impl VirtioSerialControl {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&self.port_number.to_le_bytes());
        data.extend_from_slice(&self.event.to_le_bytes());
        data.extend_from_slice(&self.value.to_le_bytes());
        data
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, io::Error> {
        if data.len() < 8 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("Data is too small {} bytes", data.len()),
            ));
        }
        let port_number = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let event = u16::from_le_bytes(data[4..6].try_into().unwrap());
        let value = u16::from_le_bytes(data[6..8].try_into().unwrap());
        Ok(Self {
            port_number,
            event,
            value,
        })
    }
}

// struct VirtioSerialControlResize {
//     columns: u16,
//     rows: u16,
// }

struct VirtioSerialControlPort {
    mem: GuestMemory,
    thread: Option<JoinHandle<()>>,
    port: Arc<VirtioSerialPort>,
}

type VirtioSerialControlReadyFn = Box<dyn Fn() + Send>;

impl VirtioSerialControlPort {
    pub fn new(mem: &GuestMemory) -> Self {
        Self {
            mem: mem.clone(),
            thread: None,
            port: Arc::new(VirtioSerialPort::new(mem)),
        }
    }

    pub fn start(&mut self, ready_callback: VirtioSerialControlReadyFn) {
        self.thread = Some(Self::start_control_thread(
            &self.port,
            &self.mem,
            ready_callback,
        ));
    }

    fn start_control_thread(
        port: &Arc<VirtioSerialPort>,
        mem: &GuestMemory,
        ready_callback: VirtioSerialControlReadyFn,
    ) -> JoinHandle<()> {
        let read_fn = port_read_fn(port, mem);
        std::thread::Builder::new()
            .name("virtio control".into())
            .spawn(move || {
                loop {
                    let data = (read_fn)();
                    if data.is_empty() {
                        break;
                    }
                    let message = VirtioSerialControl::from_bytes(&data);
                    if message.is_err() {
                        continue;
                    }

                    let message = message.unwrap();
                    match message.event {
                        VIRTIO_CONSOLE_DEVICE_READY => {
                            if message.value != 0 {
                                (ready_callback)()
                            }
                        }
                        VIRTIO_CONSOLE_PORT_READY => (),
                        _ => tracing::warn!(
                            event = message.event,
                            port_number = message.port_number,
                            value = message.value,
                            "[SERIAL] Unhandled control event",
                        ),
                    }
                }
            })
            .unwrap()
    }

    pub fn register_port(&self, port_number: u16) {
        let mut control_message = VirtioSerialControl {
            port_number: port_number as u32,
            event: VIRTIO_CONSOLE_DEVICE_ADD,
            value: 0,
        };
        self.port
            .write_all_to_port(control_message.to_bytes().as_slice());

        control_message.event = VIRTIO_CONSOLE_PORT_NAME;
        let mut name_message = control_message.to_bytes();
        name_message.extend_from_slice(format!("port{}\0", port_number).as_bytes());
        self.port.write_all_to_port(name_message.as_slice());

        control_message.event = VIRTIO_CONSOLE_PORT_OPEN;
        control_message.value = 1;
        self.port
            .write_all_to_port(control_message.to_bytes().as_slice());
    }
}

pub struct VirtioSerialDevice {
    mem: GuestMemory,
    config: VirtioSerialDeviceConfig,
    ports: Vec<Arc<VirtioSerialPort>>,
    control_port: Arc<Mutex<VirtioSerialControlPort>>,
}

type VirtioSerialPortRead = Box<dyn Fn() -> Vec<u8> + Send>;
type VirtioSerialPortWrite = Box<dyn Fn(&[u8]) + Send>;

impl VirtioSerialDevice {
    pub fn new(max_ports: u16, gm: &GuestMemory) -> Self {
        let config = VirtioSerialDeviceConfig {
            columns: 0,
            rows: 0,
            max_ports: max_ports as u32,
            _emergency_write: 0,
        };

        let control_port = Arc::new(Mutex::new(VirtioSerialControlPort::new(gm)));
        let mut ports = Vec::new();
        ports.resize_with(config.max_ports as usize, || {
            Arc::new(VirtioSerialPort::new(gm))
        });
        VirtioSerialDevice {
            mem: gm.clone(),
            config,
            ports,
            control_port,
        }
    }

    pub fn io(&self) -> SerialIo {
        SerialIo {
            ports: self.ports.clone(),
            mem: self.mem.clone(),
        }
    }
}

fn port_read_fn(port: &Arc<VirtioSerialPort>, mem: &GuestMemory) -> VirtioSerialPortRead {
    let port = port.clone();
    let mem = mem.clone();
    Box::new(move || {
        let work = port.read_from_port();
        let mut data = Vec::new();
        if let Some(work) = work {
            for payload in work.payload.iter() {
                if payload.writeable {
                    break;
                }
                data.resize(data.len() + payload.length as usize, 0);
                let dest_index = data.len() - payload.length as usize;
                let next_chunk = data.as_mut_slice().split_at_mut(dest_index).1;
                mem.read_at(payload.address, next_chunk).unwrap();
            }
            port.complete_read_from_port(work);
        }
        data
    })
}

#[derive(Clone)]
pub struct SerialIo {
    ports: Vec<Arc<VirtioSerialPort>>,
    mem: GuestMemory,
}

impl SerialIo {
    pub fn get_port_read_fn(&self, port: u16) -> VirtioSerialPortRead {
        assert!((port as usize) < self.ports.len());
        port_read_fn(&self.ports[port as usize], &self.mem)
    }

    pub fn port_write_fn(port: &Arc<VirtioSerialPort>) -> VirtioSerialPortWrite {
        let port = port.clone();
        Box::new(move |data: &[u8]| {
            port.write_all_to_port(data);
        })
    }

    pub fn get_port_write_fn(&self, port: u16) -> VirtioSerialPortWrite {
        assert!((port as usize) < self.ports.len());
        Self::port_write_fn(&self.ports[port as usize])
    }

    pub fn open_port(&self, port: u16) {
        assert!((port as usize) < self.ports.len());
        self.ports[port as usize].open();
    }

    pub fn close_port(&self, port: u16) {
        assert!((port as usize) < self.ports.len());
        self.ports[port as usize].close();
    }

    pub fn queue_input_bytes(&mut self, c: &[u8]) -> io::Result<()> {
        self.write_port(0, &c);
        Ok(())
    }

    pub fn write_port<T: AsRef<[u8]>>(&self, port: u16, data: &T) {
        self.write(port, data);
    }

    pub fn write<T>(&self, port: u16, data: &T)
    where
        T: AsRef<[u8]>,
    {
        assert!((port as usize) < self.ports.len());
        let port = &self.ports[port as usize];
        port.write_all_to_port(data.as_ref());
    }
}

impl LegacyVirtioDevice for VirtioSerialDevice {
    fn traits(&self) -> DeviceTraits {
        let queue_size = 2 + 2 * self.config.max_ports;
        let features = VirtioDeviceFeatures::new().with_bank(
            0,
            if self.config.max_ports > 1 {
                VIRTIO_CONSOLE_F_MULTIPORT
            } else {
                0
            },
        );
        DeviceTraits {
            device_id: VIRTIO_DEVICE_TYPE_CONSOLE,
            device_features: features,
            max_queues: queue_size as u16,
            device_register_length: 12,
            ..Default::default()
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        match offset {
            0 => {
                (self.config.rows as u32 & 0xff) << 24
                    | (self.config.rows as u32 >> 16) << 16
                    | (self.config.columns as u32 & 0xff) << 8
                    | (self.config.columns as u32 >> 16)
            }
            4 => self.config.max_ports,
            _ => 0,
        }
    }

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        // TODO: implement emergency_write (offset 8)
        tracing::warn!(offset, val, "[VIRTIO SERIAL] Unknown write",);
    }

    fn get_work_callback(&mut self, index: u16) -> Box<dyn VirtioQueueWorkerContext + Send> {
        let port = match index {
            0 | 1 => self.ports[0].clone(),
            2 | 3 => self.control_port.lock().port.clone(),
            _ => self.ports[index as usize / 2 - 1].clone(),
        };
        Box::new(VirtioSerialWorker {
            index,
            port,
            reader: (index & 1 == 0),
        })
    }

    fn state_change(&mut self, state: &VirtioState) {
        match state {
            // if multi-port is set, start the control port thread
            VirtioState::Running(run_state) => {
                if run_state.features.bank(0) & VIRTIO_CONSOLE_F_MULTIPORT != 0 {
                    let enabled_queues = run_state.enabled_queues.clone();
                    if run_state.enabled_queues[2] && run_state.enabled_queues[3] {
                        // on ready callback, asynchronously register the available ports with the guest.
                        let max_ports = self.config.max_ports as u16;
                        let register_control_port = self.control_port.clone();
                        self.control_port.lock().start(Box::new(move || {
                            let enabled_queues = enabled_queues.clone();
                            let register_control_port = register_control_port.clone();
                            std::thread::Builder::new()
                                .name("virtio serial register".into())
                                .spawn(move || {
                                    for port_index in 0..max_ports {
                                        let read_index = if port_index == 0 {
                                            0
                                        } else {
                                            4 + port_index as usize - 1
                                        };
                                        let write_index = if port_index == 0 {
                                            1
                                        } else {
                                            4 + port_index as usize
                                        };
                                        if enabled_queues[read_index] && enabled_queues[write_index]
                                        {
                                            register_control_port.lock().register_port(port_index);
                                        }
                                    }
                                })
                                .unwrap();
                        }));
                    }
                }
            }
            _ => {
                for port in self.ports.iter() {
                    port.stop();
                }
            }
        }
    }
}

struct VirtioSerialWorker {
    index: u16,
    port: Arc<VirtioSerialPort>,
    reader: bool,
}

#[async_trait]
impl VirtioQueueWorkerContext for VirtioSerialWorker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        if let Err(err) = work {
            tracing::error!(
                index = self.index,
                err = err.as_ref() as &dyn std::error::Error,
                "queue error"
            );
            return false;
        }
        let work = work.unwrap();
        if self.reader {
            self.port.process_virtio_read(work).await
        } else {
            self.port.process_virtio_write(work).await
        }
    }
}
