// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Debugger-mode serial relay.

use crate::SerialIo;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::future::poll_fn;
use inspect::InspectMut;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

const RX_RING_CAP: usize = 16 * 1024;
const TX_RING_CAP: usize = 16 * 1024;
const PUMP_CHUNK: usize = 1024;

/// Maximum number of productive pump iterations in a single poll before the pump
/// yields back to the executor.
///
/// This bounds how long the pump runs in one poll so that a fast, always-ready
/// backend cannot monopolize the executor. In particular, once the guest stops
/// draining and the RX ring fills, the pump must keep reading (and dropping) the
/// backend to avoid deadlocking the debugger transport; this budget ensures it
/// does so cooperatively rather than spinning without ever yielding. It is a
/// scheduler-safety guard, not a KD rate limiter (a genuine throttle is future
/// work); a backend that is synchronously always-ready can still consume CPU.
const PUMP_POLL_BUDGET: u32 = 256;

/// A [`SerialIo`] adapter for WinDbg / KD-over-serial debugger mode.
///
/// The relay keeps the wrapped backend continuously drained from an independent
/// pump task and never applies guest-visible backpressure. If either bounded
/// relay ring fills, the newest bytes that do not fit are dropped.
///
/// Because writes are accepted into a lossy relay, `poll_write` always reports
/// the full write as accepted and `poll_flush`/`poll_close` do not guarantee the
/// bytes reached the real backend. `poll_connect`/`poll_disconnect` assume a
/// single consumer (the serial emulator, which waits on only one of them at a
/// time).
pub struct DebuggerRelay {
    inner: Arc<Mutex<Inner>>,
    _pump: Task<()>,
}

struct Inner {
    rx: RingBuf,
    tx: RingBuf,
    connected: bool,
    eof: bool,
    device_rx_waker: Option<Waker>,
    device_conn_waker: Option<Waker>,
    pump_waker: Option<Waker>,
    rx_dropped: u64,
    tx_dropped: u64,
}

struct RingBuf {
    buf: VecDeque<u8>,
    cap: usize,
}

#[derive(Default)]
struct WakeList(Vec<Waker>);

impl DebuggerRelay {
    /// Wraps `inner`, spawning a pump task on `driver` to relay data in both
    /// directions. `name` labels the spawned task for diagnostics.
    pub fn new(driver: impl Spawn, name: &str, inner: Box<dyn SerialIo>) -> Self {
        let connected = inner.is_connected();
        let shared = Arc::new(Mutex::new(Inner {
            rx: RingBuf::new(RX_RING_CAP),
            tx: RingBuf::new(TX_RING_CAP),
            connected,
            eof: !connected,
            device_rx_waker: None,
            device_conn_waker: None,
            pump_waker: None,
            rx_dropped: 0,
            tx_dropped: 0,
        }));

        let pump_shared = shared.clone();
        let task_name = format!("{name}-serial-debugger-relay");
        let pump = driver.spawn(task_name, async move {
            run_pump(inner, pump_shared).await;
        });

        Self {
            inner: shared,
            _pump: pump,
        }
    }
}

impl RingBuf {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn clear(&mut self) -> usize {
        let len = self.buf.len();
        self.buf.clear();
        len
    }

    fn push_drop_newest(&mut self, data: &[u8]) -> usize {
        let available = self.cap.saturating_sub(self.buf.len());
        let accepted = available.min(data.len());
        self.buf.extend(&data[..accepted]);
        data.len() - accepted
    }

    fn pop_into(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.buf.len());
        for (dst, src) in buf.iter_mut().zip(self.buf.drain(..n)) {
            *dst = src;
        }
        n
    }

    fn copy_front(&self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.buf.len());
        for (dst, src) in buf.iter_mut().zip(self.buf.iter().copied().take(n)) {
            *dst = src;
        }
        n
    }

    fn pop_front(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        drop(self.buf.drain(..n));
    }
}

impl WakeList {
    fn take(&mut self, waker: &mut Option<Waker>) {
        if let Some(waker) = waker.take() {
            self.0.push(waker);
        }
    }

    fn wake(self) {
        for waker in self.0 {
            waker.wake();
        }
    }
}

impl Inner {
    fn connect(&mut self, wakes: &mut WakeList) -> bool {
        let changed = !self.connected || self.eof;
        self.connected = true;
        self.eof = false;
        if changed {
            wakes.take(&mut self.device_conn_waker);
            wakes.take(&mut self.device_rx_waker);
        }
        changed
    }

    fn disconnect(&mut self, wakes: &mut WakeList) -> bool {
        let was_connected = self.connected;
        let eof_changed = self.rx.is_empty() && !self.eof;
        self.connected = false;
        if self.rx.is_empty() {
            self.eof = true;
        }
        if was_connected {
            wakes.take(&mut self.device_conn_waker);
        }
        if was_connected || eof_changed {
            wakes.take(&mut self.device_rx_waker);
        }
        was_connected || eof_changed
    }

    fn wake_rx(&mut self, wakes: &mut WakeList) {
        wakes.take(&mut self.device_rx_waker);
    }

    fn wake_pump(&mut self, wakes: &mut WakeList) {
        wakes.take(&mut self.pump_waker);
    }
}

async fn run_pump(mut inner: Box<dyn SerialIo>, shared: Arc<Mutex<Inner>>) {
    let mut rx_buf = [0; PUMP_CHUNK];
    let mut tx_buf = [0; PUMP_CHUNK];

    poll_fn(move |cx| {
        let mut budget = PUMP_POLL_BUDGET;
        loop {
            let mut made_progress = false;

            if !shared.lock().connected {
                match inner.poll_connect(cx) {
                    Poll::Ready(Ok(())) => {
                        let mut wakes = WakeList::default();
                        made_progress |= shared.lock().connect(&mut wakes);
                        wakes.wake();
                    }
                    Poll::Ready(Err(_)) => {
                        let mut wakes = WakeList::default();
                        made_progress |= shared.lock().disconnect(&mut wakes);
                        wakes.wake();
                    }
                    Poll::Pending => {}
                }
            }

            if shared.lock().connected {
                match Pin::new(&mut inner).poll_read(cx, &mut rx_buf) {
                    Poll::Ready(Ok(0)) => {
                        let mut wakes = WakeList::default();
                        made_progress |= shared.lock().disconnect(&mut wakes);
                        wakes.wake();
                    }
                    Poll::Ready(Ok(n)) => {
                        let mut wakes = WakeList::default();
                        {
                            let mut state = shared.lock();
                            let dropped = state.rx.push_drop_newest(&rx_buf[..n]);
                            state.rx_dropped += dropped as u64;
                            if dropped < n {
                                state.wake_rx(&mut wakes);
                            }
                        }
                        wakes.wake();
                        made_progress = true;
                    }
                    Poll::Ready(Err(_)) => {
                        let mut wakes = WakeList::default();
                        made_progress |= shared.lock().disconnect(&mut wakes);
                        wakes.wake();
                    }
                    Poll::Pending => {}
                }
            }

            let (tx_len, had_tx) = {
                let mut state = shared.lock();
                if state.connected {
                    let len = state.tx.copy_front(&mut tx_buf);
                    (len, len != 0)
                } else {
                    let dropped = state.tx.clear();
                    state.tx_dropped += dropped as u64;
                    if dropped != 0 {
                        made_progress = true;
                    }
                    (0, dropped != 0)
                }
            };

            if tx_len != 0 {
                match Pin::new(&mut inner).poll_write(cx, &tx_buf[..tx_len]) {
                    Poll::Ready(Ok(0)) => {
                        let mut wakes = WakeList::default();
                        {
                            let mut state = shared.lock();
                            let dropped = state.tx.clear();
                            state.tx_dropped += dropped as u64;
                            made_progress |= state.disconnect(&mut wakes);
                        }
                        wakes.wake();
                    }
                    Poll::Ready(Ok(n)) => {
                        shared.lock().tx.pop_front(n);
                        made_progress = true;
                    }
                    Poll::Ready(Err(_err)) => {
                        let mut wakes = WakeList::default();
                        {
                            let mut state = shared.lock();
                            let dropped = state.tx.clear();
                            state.tx_dropped += dropped as u64;
                            made_progress |= state.disconnect(&mut wakes);
                        }
                        wakes.wake();
                    }
                    Poll::Pending => {}
                }
            }

            if made_progress {
                budget -= 1;
                if budget == 0 {
                    // Yield cooperatively so a continuously-ready backend cannot
                    // monopolize the executor. Reschedule immediately to keep
                    // draining on the next poll.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                continue;
            }

            let should_park = {
                let mut state = shared.lock();
                if !had_tx && !state.tx.is_empty() {
                    false
                } else {
                    state.pump_waker = Some(cx.waker().clone());
                    true
                }
            };
            if should_park {
                return Poll::Pending;
            }
        }
    })
    .await
}

impl SerialIo for DebuggerRelay {
    fn is_connected(&self) -> bool {
        self.inner.lock().connected
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.inner.lock();
        if state.connected {
            Poll::Ready(Ok(()))
        } else {
            state.device_conn_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.inner.lock();
        if !state.connected {
            Poll::Ready(Ok(()))
        } else {
            state.device_conn_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncRead for DebuggerRelay {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut wakes = WakeList::default();
        let result = {
            let mut state = self.inner.lock();
            if !state.rx.is_empty() || buf.is_empty() {
                let n = state.rx.pop_into(buf);
                if !state.connected && state.rx.is_empty() {
                    state.eof = true;
                }
                state.wake_pump(&mut wakes);
                Poll::Ready(Ok(n))
            } else if state.eof {
                Poll::Ready(Ok(0))
            } else {
                state.device_rx_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        };
        wakes.wake();
        result
    }
}

impl AsyncWrite for DebuggerRelay {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut wakes = WakeList::default();
        {
            let mut state = self.inner.lock();
            let dropped = state.tx.push_drop_newest(buf);
            state.tx_dropped += dropped as u64;
            state.wake_pump(&mut wakes);
        }
        wakes.wake();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl InspectMut for DebuggerRelay {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let state = self.inner.lock();
        req.respond()
            .field("connected", state.connected)
            .field("rx_depth", state.rx.len())
            .field("tx_depth", state.tx.len())
            .field("rx_dropped", state.rx_dropped)
            .field("tx_dropped", state.tx_dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::io::ErrorKind;
    use std::task::Context;
    use test_with_tracing::test;

    #[derive(Clone)]
    struct MockHandle {
        state: Arc<Mutex<MockState>>,
    }

    struct MockSerialIo {
        state: Arc<Mutex<MockState>>,
    }

    struct MockState {
        connected: bool,
        read_buf: VecDeque<u8>,
        infinite_read: bool,
        written: Vec<u8>,
        write_limit: usize,
        fail_write_disconnect: bool,
        dropped: bool,
        read_waker: Option<Waker>,
        write_waker: Option<Waker>,
        connect_waker: Option<Waker>,
        disconnect_waker: Option<Waker>,
        wait_waker: Option<Waker>,
    }

    #[derive(Debug)]
    struct RelaySnapshot {
        connected: bool,
        eof: bool,
        rx_depth: usize,
        tx_depth: usize,
        rx_dropped: u64,
        tx_dropped: u64,
    }

    impl MockSerialIo {
        fn new() -> (Self, MockHandle) {
            let state = Arc::new(Mutex::new(MockState {
                connected: true,
                read_buf: VecDeque::new(),
                infinite_read: false,
                written: Vec::new(),
                write_limit: usize::MAX,
                fail_write_disconnect: false,
                dropped: false,
                read_waker: None,
                write_waker: None,
                connect_waker: None,
                disconnect_waker: None,
                wait_waker: None,
            }));
            (
                Self {
                    state: state.clone(),
                },
                MockHandle { state },
            )
        }
    }

    impl Drop for MockSerialIo {
        fn drop(&mut self) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.dropped = true;
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }
    }

    impl InspectMut for MockSerialIo {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    impl SerialIo for MockSerialIo {
        fn is_connected(&self) -> bool {
            self.state.lock().connected
        }

        fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut state = self.state.lock();
            if state.connected {
                Poll::Ready(Ok(()))
            } else {
                state.connect_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }

        fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut state = self.state.lock();
            if state.connected {
                state.disconnect_waker = Some(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    impl AsyncRead for MockSerialIo {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let mut wakes = WakeList::default();
            let result = {
                let mut state = self.state.lock();
                if !state.connected {
                    state.wake_waiter(&mut wakes);
                    Poll::Ready(Ok(0))
                } else if state.infinite_read {
                    // A synchronously always-ready source, used to prove the pump
                    // keeps draining without spinning the executor.
                    buf.fill(0xAB);
                    state.wake_waiter(&mut wakes);
                    Poll::Ready(Ok(buf.len()))
                } else if state.read_buf.is_empty() {
                    state.read_waker = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    let n = buf.len().min(state.read_buf.len());
                    for (dst, src) in buf.iter_mut().zip(state.read_buf.drain(..n)) {
                        *dst = src;
                    }
                    state.wake_waiter(&mut wakes);
                    Poll::Ready(Ok(n))
                }
            };
            wakes.wake();
            result
        }
    }

    impl AsyncWrite for MockSerialIo {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut wakes = WakeList::default();
            let result = {
                let mut state = self.state.lock();
                if state.fail_write_disconnect {
                    state.connected = false;
                    state.wake_waiter(&mut wakes);
                    Poll::Ready(Err(ErrorKind::BrokenPipe.into()))
                } else if !state.connected {
                    Poll::Ready(Err(ErrorKind::BrokenPipe.into()))
                } else if state.write_limit == 0 {
                    state.write_waker = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    let n = state.write_limit.min(buf.len());
                    state.written.extend_from_slice(&buf[..n]);
                    state.wake_waiter(&mut wakes);
                    Poll::Ready(Ok(n))
                }
            };
            wakes.wake();
            result
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl MockState {
        fn wake_waiter(&mut self, wakes: &mut WakeList) {
            wakes.take(&mut self.wait_waker);
        }
    }

    impl MockHandle {
        fn inject_rx(&self, data: &[u8]) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.read_buf.extend(data);
                wakes.take(&mut state.read_waker);
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }

        fn set_infinite_read(&self) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.infinite_read = true;
                wakes.take(&mut state.read_waker);
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }

        fn set_connected(&self, connected: bool) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.connected = connected;
                if connected {
                    wakes.take(&mut state.connect_waker);
                } else {
                    wakes.take(&mut state.disconnect_waker);
                    wakes.take(&mut state.read_waker);
                    wakes.take(&mut state.write_waker);
                }
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }

        fn set_write_limit(&self, limit: usize) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.write_limit = limit;
                wakes.take(&mut state.write_waker);
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }

        fn fail_writes_with_disconnect(&self) {
            let mut wakes = WakeList::default();
            {
                let mut state = self.state.lock();
                state.fail_write_disconnect = true;
                wakes.take(&mut state.write_waker);
                state.wake_waiter(&mut wakes);
            }
            wakes.wake();
        }

        fn written(&self) -> Vec<u8> {
            self.state.lock().written.clone()
        }

        async fn wait_until(&self, mut predicate: impl FnMut(&MockState) -> bool) {
            poll_fn(|cx| {
                let mut state = self.state.lock();
                if predicate(&state) {
                    Poll::Ready(())
                } else {
                    state.wait_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await
        }
    }

    fn sequence(len: usize) -> Vec<u8> {
        (0..len).map(|x| (x % 251) as u8).collect()
    }

    fn snapshot(relay: &DebuggerRelay) -> RelaySnapshot {
        let state = relay.inner.lock();
        RelaySnapshot {
            connected: state.connected,
            eof: state.eof,
            rx_depth: state.rx.len(),
            tx_depth: state.tx.len(),
            rx_dropped: state.rx_dropped,
            tx_dropped: state.tx_dropped,
        }
    }

    async fn wait_for_relay(
        relay: &DebuggerRelay,
        mut predicate: impl FnMut(&RelaySnapshot) -> bool,
    ) {
        poll_fn(|cx| {
            if predicate(&snapshot(relay)) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    fn poll_read_now(relay: &mut DebuggerRelay, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(relay).poll_read(&mut cx, buf)
    }

    fn poll_write_now(relay: &mut DebuggerRelay, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(relay).poll_write(&mut cx, buf)
    }

    fn poll_connect_now(relay: &mut DebuggerRelay) -> Poll<io::Result<()>> {
        let mut cx = Context::from_waker(Waker::noop());
        relay.poll_connect(&mut cx)
    }

    #[async_test]
    async fn rx_drains_without_reader(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        let relay = DebuggerRelay::new(driver, "rx-drain", Box::new(io));
        let burst = sequence(RX_RING_CAP + 512);

        handle.inject_rx(&burst);
        handle.wait_until(|state| state.read_buf.is_empty()).await;

        let snapshot = snapshot(&relay);
        assert_eq!(snapshot.rx_depth, RX_RING_CAP);
        assert_eq!(snapshot.rx_dropped, 512);
    }

    /// With a synchronously always-ready backend and a full RX ring, the pump
    /// must keep reading and dropping (so the transport never deadlocks) but must
    /// still yield back to the executor rather than spinning forever in a single
    /// poll. Polling `run_pump` directly asserts this deterministically: a fixed
    /// pump returns `Pending` (after bounded work) and re-arms its own waker; a
    /// spinning pump would never return from this single poll (timeout).
    #[test]
    fn pump_yields_instead_of_spinning() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::task::Wake;

        struct CountingWaker(AtomicUsize);
        impl Wake for CountingWaker {
            fn wake(self: StdArc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn wake_by_ref(self: &StdArc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let shared = Arc::new(Mutex::new(Inner {
            rx: RingBuf::new(RX_RING_CAP),
            tx: RingBuf::new(TX_RING_CAP),
            connected: true,
            eof: false,
            device_rx_waker: None,
            device_conn_waker: None,
            pump_waker: None,
            rx_dropped: 0,
            tx_dropped: 0,
        }));

        let (io, handle) = MockSerialIo::new();
        handle.set_infinite_read();

        let fut = run_pump(Box::new(io), shared.clone());
        let mut fut = std::pin::pin!(fut);

        let counter = StdArc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        // A single poll must return: the pump drains and drops into the full ring
        // but yields once its per-poll budget is spent. (If it spun, this poll
        // would never return and the test would time out.)
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // It kept draining/dropping the always-ready backend within that poll...
        assert!(shared.lock().rx_dropped >= RX_RING_CAP as u64);
        // ...and re-armed itself to continue on the next poll (cooperative yield).
        assert!(counter.0.load(Ordering::Relaxed) >= 1);
    }

    #[async_test]
    async fn rx_fifo_preserves_oldest_and_drops_newest(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        let mut relay = DebuggerRelay::new(driver, "rx-drop", Box::new(io));
        let burst = sequence(RX_RING_CAP + 123);

        handle.inject_rx(&burst);
        handle.wait_until(|state| state.read_buf.is_empty()).await;

        let mut delivered = vec![0; RX_RING_CAP];
        assert!(matches!(
            poll_read_now(&mut relay, &mut delivered),
            Poll::Ready(Ok(RX_RING_CAP))
        ));
        assert_eq!(delivered, burst[..RX_RING_CAP]);
        assert_eq!(snapshot(&relay).rx_dropped, 123);
    }

    #[async_test]
    async fn rx_eof_and_reconnect(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        let mut relay = DebuggerRelay::new(driver, "rx-eof", Box::new(io));

        handle.inject_rx(&[1, 2, 3]);
        handle.wait_until(|state| state.read_buf.is_empty()).await;
        handle.set_connected(false);
        wait_for_relay(&relay, |state| !state.connected).await;

        let mut delivered = [0; 8];
        assert!(matches!(
            poll_read_now(&mut relay, &mut delivered),
            Poll::Ready(Ok(3))
        ));
        assert_eq!(&delivered[..3], &[1, 2, 3]);
        assert!(matches!(
            poll_read_now(&mut relay, &mut delivered),
            Poll::Ready(Ok(0))
        ));
        assert!(matches!(poll_connect_now(&mut relay), Poll::Pending));

        handle.set_connected(true);
        wait_for_relay(&relay, |state| state.connected && !state.eof).await;
        assert!(matches!(poll_connect_now(&mut relay), Poll::Ready(Ok(()))));
        assert!(relay.is_connected());
    }

    #[async_test]
    async fn tx_never_blocks_and_drops_overflow(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        handle.set_write_limit(0);
        let mut relay = DebuggerRelay::new(driver, "tx-drop", Box::new(io));
        let chunk = [0xa5; 1024];

        for _ in 0..=TX_RING_CAP / chunk.len() {
            assert!(matches!(
                poll_write_now(&mut relay, &chunk),
                Poll::Ready(Ok(1024))
            ));
        }

        let snapshot = snapshot(&relay);
        assert!(snapshot.tx_depth <= TX_RING_CAP);
        assert!(snapshot.tx_dropped > 0);
    }

    #[async_test]
    async fn tx_forwards_in_fifo_order(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        handle.set_write_limit(3);
        let mut relay = DebuggerRelay::new(driver, "tx-forward", Box::new(io));
        let data = sequence(97);

        assert!(matches!(
            poll_write_now(&mut relay, &data),
            Poll::Ready(Ok(97))
        ));
        handle
            .wait_until(|state| state.written.len() == data.len())
            .await;

        assert_eq!(handle.written(), data);
    }

    #[async_test]
    async fn tx_error_disconnects_without_blocking(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        handle.fail_writes_with_disconnect();
        let mut relay = DebuggerRelay::new(driver, "tx-error", Box::new(io));

        assert!(matches!(
            poll_write_now(&mut relay, b"debug"),
            Poll::Ready(Ok(5))
        ));
        wait_for_relay(&relay, |state| !state.connected).await;
        assert!(!relay.is_connected());
        assert!(matches!(
            poll_write_now(&mut relay, b"still accepted"),
            Poll::Ready(Ok(14))
        ));
    }

    #[async_test]
    async fn dropping_relay_cancels_pump_and_drops_inner(driver: DefaultDriver) {
        let (io, handle) = MockSerialIo::new();
        let relay = DebuggerRelay::new(driver, "teardown", Box::new(io));

        drop(relay);
        handle.wait_until(|state| state.dropped).await;
    }
}
