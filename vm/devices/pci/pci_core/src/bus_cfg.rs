// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for originating PCI configuration space accesses in bus emulators.

use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigByteEnable;
use inspect::Inspect;
use std::task::Context;
use std::task::Poll;
use zerocopy::IntoBytes;

/// Callback trait for the [`PciBusCfgAccessHandler`] for bus-specific operations.
pub trait PciBusCfgAccessCallbacks {
    /// Dispatches a read to the downstream config-space target.
    fn read(&mut self, addr: PciConfigAddress, value: ByteEnabledDwordRead<'_>) -> IoResult;

    /// Dispatches a write to the downstream config-space target.
    fn write(&mut self, addr: PciConfigAddress, value: ByteEnabledDwordWrite) -> IoResult;
}

/// A pending config space access that was deferred by a downstream device.
#[derive(Inspect)]
#[inspect(tag = "kind")]
enum DeferredCfgAccess {
    /// A read that was deferred by a downstream device.
    Read {
        #[inspect(skip)]
        deferred_device_read: DeferredToken,
        #[inspect(skip)]
        bus_read: DeferredRead,
        addr: PciConfigAddress,
        byte_enable: PciConfigByteEnable,
    },
    /// A write that was deferred by a downstream device.
    Write {
        #[inspect(skip)]
        deferred_device_write: DeferredToken,
        #[inspect(skip)]
        bus_write: DeferredWrite,
        addr: PciConfigAddress,
    },
}

/// A handler for managing PCI config space accesses to downstream devices.
#[derive(Default, Inspect)]
pub struct PciBusCfgAccessHandler {
    #[inspect(with = "|x| x.is_some()")]
    waker: Option<std::task::Waker>,
    #[inspect(iter_by_index)]
    actions: Vec<DeferredCfgAccess>,
}

impl PciBusCfgAccessHandler {
    /// Creates an empty deferred-access tracker.
    pub fn new() -> Self {
        Self {
            waker: None,
            actions: Vec::new(),
        }
    }

    /// Returns whether there are no deferred accesses pending.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Handles a config space read request.
    pub fn read(
        &mut self,
        addr: PciConfigAddress,
        mut inline_completion_value: ByteEnabledDwordRead<'_>,
        callbacks: &mut impl PciBusCfgAccessCallbacks,
    ) -> IoResult {
        match callbacks.read(addr, inline_completion_value.reborrow()) {
            IoResult::Ok => IoResult::Ok,
            IoResult::Err(err) => IoResult::Err(err),
            IoResult::Defer(deferred_device_read) => {
                let (bus_read, bus_token) = defer_read();
                self.push_action(DeferredCfgAccess::Read {
                    deferred_device_read,
                    bus_read,
                    addr,
                    byte_enable: inline_completion_value.byte_enable(),
                });
                IoResult::Defer(bus_token)
            }
        }
    }

    /// Handles a config space write request.
    pub fn write(
        &mut self,
        addr: PciConfigAddress,
        value: ByteEnabledDwordWrite,
        callbacks: &mut impl PciBusCfgAccessCallbacks,
    ) -> IoResult {
        let result = callbacks.write(addr, value);
        if let IoResult::Defer(deferred_device_write) = result {
            let (bus_write, bus_token) = defer_write();
            self.push_action(DeferredCfgAccess::Write {
                deferred_device_write,
                bus_write,
                addr,
            });
            return IoResult::Defer(bus_token);
        }

        result
    }

    /// Polls pending accesses and keeps any that are still incomplete.
    pub fn poll(&mut self, cx: &mut Context<'_>) {
        self.waker = Some(cx.waker().clone());
        self.actions = std::mem::take(&mut self.actions)
            .into_iter()
            .filter_map(|action| match action {
                DeferredCfgAccess::Read {
                    mut deferred_device_read,
                    bus_read,
                    addr,
                    byte_enable,
                } => {
                    // If the inner read is ready, complete the outer read accordingly.
                    let mut dword_buffer = 0;
                    if let Poll::Ready(res) =
                        deferred_device_read.poll_read(cx, dword_buffer.as_mut_bytes())
                    {
                        match res {
                            Ok(()) => {
                                let (byte_offset, len) = byte_enable.to_byte_offset_len();
                                let byte_offset = byte_offset as usize;
                                bus_read.complete(
                                    &dword_buffer.as_bytes()[byte_offset..byte_offset + len],
                                );
                            }
                            Err(err) => bus_read.complete_error(err),
                        }
                        None
                    } else {
                        // If the inner read is not ready, keep the outer read pending and
                        // leave the deferred action in the list for the next poll.
                        Some(DeferredCfgAccess::Read {
                            deferred_device_read,
                            bus_read,
                            addr,
                            byte_enable,
                        })
                    }
                }
                DeferredCfgAccess::Write {
                    mut deferred_device_write,
                    bus_write,
                    addr,
                } => {
                    // If the inner write completed, complete the outer write accordingly.
                    if let Poll::Ready(res) = deferred_device_write.poll_write(cx) {
                        match res {
                            Ok(()) => bus_write.complete(),
                            Err(err) => bus_write.complete_error(err),
                        }
                        None
                    } else {
                        // If the inner write is not ready, keep the outer write pending and
                        // leave the deferred action in the list for the next poll.
                        Some(DeferredCfgAccess::Write {
                            deferred_device_write,
                            bus_write,
                            addr,
                        })
                    }
                }
            })
            .collect();
    }

    fn push_action(&mut self, action: DeferredCfgAccess) {
        self.actions.push(action);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::io::IoError;

    #[derive(Clone, Copy)]
    enum ReadAction {
        Ok(u32),
        Err(IoError),
        Defer,
    }

    #[derive(Clone, Copy)]
    enum WriteAction {
        Ok,
        Err(IoError),
        Defer,
    }

    struct DeferredCallbacks {
        read_action: ReadAction,
        write_action: WriteAction,
        pending_read: Option<DeferredRead>,
        pending_write: Option<DeferredWrite>,
        reads: Vec<PciConfigAddress>,
        writes: Vec<(PciConfigAddress, u32)>,
    }

    impl DeferredCallbacks {
        fn new(read_action: ReadAction, write_action: WriteAction) -> Self {
            Self {
                read_action,
                write_action,
                pending_read: None,
                pending_write: None,
                reads: Vec::new(),
                writes: Vec::new(),
            }
        }

        fn complete_read(&mut self, value: u32) {
            self.pending_read
                .take()
                .unwrap()
                .complete(&value.as_bytes()[..4]);
        }

        fn complete_read_error(&mut self, error: IoError) {
            self.pending_read.take().unwrap().complete_error(error);
        }

        fn complete_write(&mut self) {
            self.pending_write.take().unwrap().complete();
        }

        fn complete_write_error(&mut self, error: IoError) {
            self.pending_write.take().unwrap().complete_error(error);
        }
    }

    impl PciBusCfgAccessCallbacks for DeferredCallbacks {
        fn read(
            &mut self,
            addr: PciConfigAddress,
            mut value: ByteEnabledDwordRead<'_>,
        ) -> IoResult {
            self.reads.push(addr);
            match self.read_action {
                ReadAction::Ok(read_value) => {
                    value.set(read_value);
                    IoResult::Ok
                }
                ReadAction::Err(error) => IoResult::Err(error),
                ReadAction::Defer => {
                    let (deferred, token) = defer_read();
                    assert!(self.pending_read.replace(deferred).is_none());
                    IoResult::Defer(token)
                }
            }
        }

        fn write(&mut self, addr: PciConfigAddress, value: ByteEnabledDwordWrite) -> IoResult {
            self.writes.push((addr, value.extract()));
            match self.write_action {
                WriteAction::Ok => IoResult::Ok,
                WriteAction::Err(error) => IoResult::Err(error),
                WriteAction::Defer => {
                    let (deferred, token) = defer_write();
                    assert!(self.pending_write.replace(deferred).is_none());
                    IoResult::Defer(token)
                }
            }
        }
    }

    fn poll_once(handler: &mut PciBusCfgAccessHandler) {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        handler.poll(&mut cx);
    }

    fn poll_read_token(token: &mut DeferredToken, bytes: &mut [u8]) -> Poll<Result<(), IoError>> {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        token.poll_read(&mut cx, bytes)
    }

    fn poll_write_token(token: &mut DeferredToken) -> Poll<Result<(), IoError>> {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        token.poll_write(&mut cx)
    }

    #[test]
    fn immediate_read_applies_byte_enable() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Ok(0x1122_3344), WriteAction::Ok);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let mut buffer = 0xffff_ffff;
        let value = ByteEnabledDwordRead::new(&mut buffer, PciConfigByteEnable::HIGH_WORD);

        assert!(matches!(
            handler.read(addr, value, &mut callbacks),
            IoResult::Ok
        ));
        assert_eq!(callbacks.reads, vec![addr]);
        assert_eq!(buffer, 0x1122_ffff);
    }

    #[test]
    fn immediate_read_error_is_returned() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks =
            DeferredCallbacks::new(ReadAction::Err(IoError::InvalidRegister), WriteAction::Ok);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let mut buffer = 0;
        let value = ByteEnabledDwordRead::with_all_bytes_enabled(&mut buffer);

        assert!(matches!(
            handler.read(addr, value, &mut callbacks),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert_eq!(callbacks.reads, vec![addr]);
    }

    #[test]
    fn deferred_read_applies_byte_enable() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Defer, WriteAction::Ok);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let mut buffer = 0xffff_ffff;
        let value = ByteEnabledDwordRead::new(&mut buffer, PciConfigByteEnable::HIGH_WORD);

        let IoResult::Defer(mut bus_token) = handler.read(addr, value, &mut callbacks) else {
            panic!("read should defer");
        };

        callbacks.complete_read(0x1122_3344);
        poll_once(&mut handler);

        let mut read_data = [0; 2];
        assert!(matches!(
            poll_read_token(&mut bus_token, &mut read_data),
            Poll::Ready(Ok(()))
        ));

        assert_eq!(read_data, [0x22, 0x11]);
    }

    #[test]
    fn deferred_read_error_completes_outer_read_with_error() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Defer, WriteAction::Ok);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let mut buffer = 0;
        let value = ByteEnabledDwordRead::with_all_bytes_enabled(&mut buffer);

        let IoResult::Defer(mut bus_token) = handler.read(addr, value, &mut callbacks) else {
            panic!("read should defer");
        };

        callbacks.complete_read_error(IoError::NoResponse);
        poll_once(&mut handler);

        let mut read_data = [0; 4];
        assert!(matches!(
            poll_read_token(&mut bus_token, &mut read_data),
            Poll::Ready(Err(IoError::NoResponse))
        ));
    }

    #[test]
    fn partial_writes_do_not_read_for_write() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Ok(0x1122_3344), WriteAction::Ok);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let write_value = ByteEnabledDwordWrite::new(0xaa00, PciConfigByteEnable::BYTE1);

        assert!(matches!(
            handler.write(addr, write_value, &mut callbacks),
            IoResult::Ok
        ));
        assert_eq!(callbacks.reads, vec![]);
        assert_eq!(callbacks.writes, vec![(addr, 0x0000_aa00)]);
    }

    #[test]
    fn immediate_write_error_is_returned() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(
            ReadAction::Ok(0x1122_3344),
            WriteAction::Err(IoError::InvalidRegister),
        );
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let write_value = ByteEnabledDwordWrite::new(0xaa00, PciConfigByteEnable::BYTE1);

        assert!(matches!(
            handler.write(addr, write_value, &mut callbacks),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert_eq!(callbacks.writes, vec![(addr, 0x0000_aa00)]);
    }

    #[test]
    fn deferred_writes_complete_outer_write() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Ok(0), WriteAction::Defer);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let write_value = ByteEnabledDwordWrite::with_all_bytes_enabled(0xaabb_ccdd);

        let IoResult::Defer(mut bus_token) = handler.write(addr, write_value, &mut callbacks)
        else {
            panic!("write should defer");
        };

        callbacks.complete_write();
        poll_once(&mut handler);

        assert!(matches!(
            poll_write_token(&mut bus_token),
            Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn deferred_write_error_completes_outer_write_with_error() {
        let mut handler = PciBusCfgAccessHandler::new();
        let mut callbacks = DeferredCallbacks::new(ReadAction::Ok(0), WriteAction::Defer);
        let addr = PciConfigAddress::new(0, 0, 1).unwrap();
        let write_value = ByteEnabledDwordWrite::with_all_bytes_enabled(0xaabb_ccdd);

        let IoResult::Defer(mut bus_token) = handler.write(addr, write_value, &mut callbacks)
        else {
            panic!("write should defer");
        };

        callbacks.complete_write_error(IoError::NoResponse);
        poll_once(&mut handler);

        assert!(matches!(
            poll_write_token(&mut bus_token),
            Poll::Ready(Err(IoError::NoResponse))
        ));
    }
}
