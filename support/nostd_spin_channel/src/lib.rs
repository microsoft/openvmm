#![no_std]
extern crate alloc;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::MutexGuard;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use spin::Mutex;
use thiserror::Error;

/// An unbounded channel implementation with priority send capability.
/// This implementation works in no_std environments using spin-rs.
/// It uses a VecDeque as the underlying buffer and provides blocking and non-blocking interfaces.
pub struct Channel<T> {
    inner: Arc<ChannelInner<T>>,
}

/// The inner data structure holding the channel state
struct ChannelInner<T> {
    /// The internal buffer using a VecDeque protected by its own mutex
    buffer: Mutex<VecDeque<T>>,

    /// Number of active senders
    senders: AtomicUsize,

    /// Number of active receivers
    receivers: AtomicUsize,
}

/// Error type for sending operations
#[derive(Debug, Eq, PartialEq, Error)]
pub enum SendError {
    /// All receivers have been dropped
    #[error("send failed because receiver is disconnected")]
    Disconnected,
}

/// Error type for receiving operations
#[derive(Debug, Eq, PartialEq, Error)]
pub enum RecvError {
    #[error("receive failed because channel is empty")]
    Empty,
    #[error("receive failed because all senders are disconnected")]
    Disconnected,
    #[error("channel is locked by another operation")]
    Unavailable
}

/// Sender half of the channel
pub struct Sender<T> {
    inner: Arc<ChannelInner<T>>,
}

/// Receiver half of the channel
pub struct Receiver<T> {
    inner: Arc<ChannelInner<T>>,
}

// implement clone for Sender
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.senders.fetch_add(1, Ordering::SeqCst);
        Sender {
            inner: self.inner.clone(),
        }
    }
}

// implement clone for Receiver
impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.inner.receivers.fetch_add(1, Ordering::SeqCst);
        Receiver {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Channel<T> {
    /// Creates a new unbounded channel
    pub fn new() -> Self {
        let inner = Arc::new(ChannelInner {
            buffer: Mutex::new(VecDeque::new()),
            senders: AtomicUsize::new(1),   // Start with one sender
            receivers: AtomicUsize::new(1), // Start with one receiver
        });

        Self { inner }
    }

    /// Splits the channel into a sender and receiver pair
    pub fn split(self) -> (Sender<T>, Receiver<T>) {
        let sender = Sender {
            inner: self.inner.clone(),
        };

        let receiver = Receiver { inner: self.inner };

        (sender, receiver)
    }

    /// Returns the current number of elements in the channel
    pub fn len(&self) -> usize {
        self.inner.buffer.lock().len()
    }

    /// Returns true if the channel is empty
    pub fn is_empty(&self) -> bool {
        self.inner.buffer.lock().is_empty()
    }
}

impl<T> Sender<T> {
    /// Sends an element to the back of the queue
    /// Returns Ok(()) if successful, Err(SendError) if all receivers have been dropped
    pub fn send(&self, value: T) -> Result<(), SendError> {
        let mut buffer = self.buffer()?;
        // Push to the back of the queue - can't fail since we're unbounded
        buffer.push_back(value);

        Ok(())
    }

    /// Sends an element to the front of the queue (highest priority)
    /// Returns Ok(()) if successful, Err(SendError) if all receivers have been dropped
    pub fn send_priority(&self, value: T) -> Result<(), SendError> {
        let mut buffer = self.buffer()?;

        // Push to the front of the queue - can't fail since we're unbounded
        buffer.push_front(value);

        Ok(())
    }

    /// Send a batch of elements at once
    /// Returns the number of elements successfully sent (all of them, unless disconnected)
    pub fn send_batch<I>(&self, items: I) -> Result<usize, SendError>
    where
        I: IntoIterator<Item = T>,
    {
        let mut buffer = self.buffer()?;

        let mut count = 0;

        // Push each item to the back of the queue
        for item in items {
            buffer.push_back(item);
            count += 1;
        }

        Ok(count)
    }

    /// Returns the current number of elements in the channel
    pub fn len(&self) -> usize {
        self.inner.buffer.lock().len()
    }

    /// Returns true if the channel is empty
    pub fn is_empty(&self) -> bool {
        self.inner.buffer.lock().is_empty()
    }

    fn buffer(&self) -> Result<MutexGuard<'_, VecDeque<T>>, SendError> {
        if self.inner.receivers.load(Ordering::SeqCst) == 0 {
            return Err(SendError::Disconnected);
        }
        let buffer = self.inner.buffer.lock();
        Ok(buffer)
    }
}

impl<T> Receiver<T> {
    /// Tries to receive an element from the front of the queue while blocking
    /// Returns Ok(value) if successful, Err(RecvError) otherwise
    pub fn recv(&self) -> Result<T, RecvError> {
        // Use a separate scope for the lock to ensure it's released promptly
        let result = {
            let mut buffer = self.inner.buffer.lock();
            buffer.pop_front()
        };
         match result {
            Some(val) => Ok(val),
            None => {
                // Check if there are any senders left
                if self.inner.senders.load(Ordering::SeqCst) == 0 {
                    Err(RecvError::Disconnected)
                } else {
                    Err(RecvError::Empty)
                }
            }
        }
    }

    /// Tries to receive an element from the front of the queue without blocking
    /// Returns Ok(value) if successful, Err(RecvError) otherwise
    pub fn try_recv(&self) -> Result<T, RecvError> {
        // Use a separate scope for the lock to ensure it's released promptly
        let result = {
            let mut buffer = self.inner.buffer.try_lock();
            if buffer.is_none() {
                return Err(RecvError::Unavailable);
            }
            buffer.as_mut().unwrap().pop_front()
        };

        match result {
            Some(val) => Ok(val),
            None => {
                // Check if there are any senders left
                if self.inner.senders.load(Ordering::SeqCst) == 0 {
                    Err(RecvError::Disconnected)
                } else {
                    Err(RecvError::Empty)
                }
            }
        }
    }

    /// Tries to receive multiple elements at once, up to the specified limit
    /// Returns a vector of received elements
    pub fn try_recv_batch(&self, max_items: usize) -> Vec<T>
    where
        T: Send,
    {
        // If max_items is 0, return an empty vector
        if max_items == 0 {
            return Vec::new();
        }

        let mut items = Vec::new();

        // Lock the buffer once for the entire batch
        let mut buffer = self.inner.buffer.lock();

        // Calculate how many items to take
        let count = max_items.min(buffer.len());

        // Reserve capacity for efficiency
        items.reserve(count);

        // Take items from the front of the queue
        for _ in 0..count {
            if let Some(item) = buffer.pop_front() {
                items.push(item);
            } else {
                // This shouldn't happen due to the min() above, but just in case
                break;
            }
        }

        items
    }

    /// Peeks at the next element without removing it
    pub fn peek(&self) -> Option<T>
    where
        T: Clone,
    {
        let buffer = self.inner.buffer.lock();
        buffer.front().cloned()
    }

    /// Returns the current number of elements in the channel
    pub fn len(&self) -> usize {
        self.inner.buffer.lock().len()
    }

    /// Returns true if the channel is empty
    pub fn is_empty(&self) -> bool {
        self.inner.buffer.lock().is_empty()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.senders.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.receivers.fetch_sub(1, Ordering::SeqCst);
    }
}
