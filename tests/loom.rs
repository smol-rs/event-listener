#![cfg(loom)]

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use event_listener::{Event, EventListener};
use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;
use std::ops;

/// A basic MPMC channel based on a ConcurrentQueue, Event, and loom primitives.
struct Channel<T> {
    /// The queue used to contain items.
    queue: ConcurrentQueue<T>,

    /// The number of senders.
    senders: AtomicUsize,

    /// The number of receivers.
    receivers: AtomicUsize,

    /// The event that is signaled when a new item is pushed.
    push_event: Event,

    /// The event that is signaled when a new item is popped.
    pop_event: Event,
}

/// The sending side of a channel.
struct Sender<T> {
    /// The channel.
    channel: Arc<Channel<T>>,
}

/// The receiving side of a channel.
struct Receiver<T> {
    /// The channel.
    channel: Arc<Channel<T>>,
}

/// Create a new pair of senders/receivers based on a queue.
fn pair<T>() -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel {
        queue: ConcurrentQueue::bounded(10),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        push_event: Event::new(),
        pop_event: Event::new(),
    });

    (
        Sender {
            channel: channel.clone(),
        },
        Receiver { channel },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.channel.senders.fetch_add(1, Ordering::SeqCst);
        Sender {
            channel: self.channel.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.channel.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Close the channel and notify the receivers.
            self.channel.queue.close();
            self.channel.push_event.notify_additional(core::usize::MAX);
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.channel.receivers.fetch_add(1, Ordering::SeqCst);
        Receiver {
            channel: self.channel.clone(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.channel.receivers.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Close the channel and notify the senders.
            self.channel.queue.close();
            self.channel.pop_event.notify_additional(core::usize::MAX);
        }
    }
}

impl<T> Sender<T> {
    /// Send a value.
    ///
    /// Returns an error with the value if the channel is closed.
    fn send(&self, mut value: T) -> Result<(), T> {
        let mut listener = None;

        loop {
            match self.channel.queue.push(value) {
                Ok(()) => {
                    // Notify a single receiver.
                    self.channel.push_event.notify_additional(1);
                    return Ok(());
                }
                Err(PushError::Closed(val)) => return Err(val),
                Err(PushError::Full(val)) => {
                    // Wait for a receiver to pop an item.
                    match listener.take() {
                        Some(listener) => listener.wait(),
                        None => {
                            listener = Some(self.channel.pop_event.listen());
                        }
                    }
                }
            }
        }
    }
}

impl<T> Receiver<T> {
    /// Receive a value.
    ///
    /// Returns an error if the channel is closed.
    fn recv(&self) -> Result<T, ()> {
        let mut listener = None;

        loop {
            match self.channel.queue.pop() {
                Ok(value) => {
                    // Notify a single sender.
                    self.channel.pop_event.notify_additional(1);
                    return Ok(value);
                }
                Err(PopError::Closed) => return Err(()),
                Err(PopError::Empty) => {
                    // Wait for a sender to push an item.
                    match listener.take() {
                        Some(listener) => listener.wait(),
                        None => {
                            listener = Some(self.channel.push_event.listen());
                        }
                    }
                }
            }
        }
    }
}

/// A basic Mutex based on Event and loom primitives.
struct Mutex<T> {
    /// The inner value.
    value: UnsafeCell<T>,

    /// The event that is signaled when the value is unlocked.
    event: Event,

    /// Is this mutex locked?
    locked: AtomicBool,
}

/// A guard that unlocks the mutex when dropped.
struct MutexGuard<'a, T> {
    /// The mutex.
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    /// Create a new mutex.
    fn new(value: T) -> Mutex<T> {
        Mutex {
            value: UnsafeCell::new(value),
            event: Event::new(),
            locked: AtomicBool::new(false),
        }
    }

    /// Lock the mutex.
    fn lock(&self) -> MutexGuard<'_, T> {
        let mut listener = None;

        loop {
            match self
                .locked
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return MutexGuard { mutex: self },
                Err(_) => {
                    // Wait for the mutex to be unlocked.
                    match listener.take() {
                        Some(listener) => listener.wait(),
                        None => {
                            listener = Some(self.event.listen());
                        }
                    }
                }
            }
        }
    }

    /// Get the inner value.
    fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<'a, T> MutexGuard<'a, T> {
    fn with<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        f(unsafe { &mut *self.mutex.value.get() })
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::SeqCst);
        self.mutex.event.notify(1);
    }
}

#[test]
fn spsc() {
    loom::model(|| {
        // Create a new pair of senders/receivers.
        let (tx, rx) = pair();

        // Push each onto a thread and run them.
        let handle = thread::spawn(move || {
            for i in 0..limit {
                if tx.send(i).is_err() {
                    break;
                }
            }
        });

        let mut recv_values = vec![];

        loop {
            match rx.recv() {
                Ok(value) => recv_values.push(value),
                Err(()) => break,
            }
        }

        // Values may not be in order.
        recv_values.sort_unstable();
        assert_eq!(recv_values, (0..limit).collect::<Vec<_>>());

        // Join the handle before we exit.
        handle.join().unwrap();
    });
}

#[test]
fn contended_mutex() {
    loom::model(|| {
        // Create a new mutex.
        let mutex = Arc::new(Mutex::new(0));

        // Create a bunch of threads that increment the mutex.
        let mut handles = vec![];
        let num_threads = loom::MAX_THREADS - 1;

        for _ in 0..num_threads {
            let mutex = mutex.clone();
            let handle = thread::spawn(move || {
                let mut mutex = mutex.lock();
                mutex.with(|val| {
                    *val += 1;
                });
            });
            handles.push(handle);
        }

        // Join the handles.
        for handle in handles {
            handle.join().unwrap();
        }

        // The mutex should have the correct value.
        let value = Arc::try_unwrap(mutex)
            .unwrap_or_else(|_| panic!("mutex is locked"))
            .into_inner();
        assert_eq!(value, num_threads);
    });
}
