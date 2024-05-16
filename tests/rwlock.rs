//! Emulate the async-lock RWLock using event-listener as a test.

#![allow(unused_must_use)]

use event_listener::{listener, Event, EventListener};
use futures_lite::{future, prelude::*, ready};

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;

const WRITER_BIT: usize = 1;
const ONE_READER: usize = 2;

struct CallOnDrop<F: FnMut()>(F);
impl<F: FnMut()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[cfg(not(target_family = "wasm"))]
fn spawn<T: Send + 'static>(f: impl Future<Output = T> + Send + 'static) -> future::Boxed<T> {
    let (s, r) = flume::bounded(1);
    std::thread::spawn(move || {
        future::block_on(async {
            let _ = s.send_async(f.await).await;
        })
    });
    async move { r.recv_async().await.unwrap() }.boxed()
}

/// Modeled after `async_lock::Mutex<()>`.
///
/// The main difference is that there is not any contention management. Hopefully this shouldn't
/// matter that much.
struct Mutex {
    locked: AtomicBool,
    lock_ops: Event,
}

impl Mutex {
    /// Create a new mutex.
    fn new() -> Self {
        Mutex {
            locked: AtomicBool::new(false),
            lock_ops: Event::new(),
        }
    }

    /// Try to lock this mutex.
    fn try_lock(&self) -> Option<MutexGuard<'_>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(MutexGuard(self))
        } else {
            None
        }
    }

    /// Wait on locking this mutex.
    async fn lock(&self) -> MutexGuard<'_> {
        loop {
            if let Some(lock) = self.try_lock() {
                return lock;
            }

            listener!(self.lock_ops => listener);

            if let Some(lock) = self.try_lock() {
                return lock;
            }

            listener.await;
        }
    }
}

/// RAII guard for a Mutex.
struct MutexGuard<'a>(&'a Mutex);

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
        self.0.lock_ops.notify(1);
    }
}

/// Modeled after `async_lock::RwLock<()>`.
struct RwLock {
    /// Acquired by the writer.
    mutex: Mutex,

    /// Event triggered when the last reader is dropped.
    no_readers: Event,

    /// Event triggered when the writer is dropped.
    no_writer: Event,

    /// Current state of the lock.
    ///
    /// The least significant bit (`WRITER_BIT`) is set to 1 when a writer is holding the lock or
    /// trying to acquire it.
    ///
    /// The upper bits contain the number of currently active readers. Each active reader
    /// increments the state by `ONE_READER`.
    state: AtomicUsize,
}

impl RwLock {
    fn new() -> Self {
        RwLock {
            mutex: Mutex::new(),
            no_readers: Event::new(),
            no_writer: Event::new(),
            state: AtomicUsize::new(0),
        }
    }

    async fn read(&self) -> ReadGuard<'_> {
        let mut state = self.state.load(Ordering::Acquire);
        let mut listener: Option<EventListener> = None;

        future::poll_fn(|cx| {
            loop {
                if state & WRITER_BIT == 0 {
                    // Make sure the number of readers doesn't overflow.
                    if state > core::isize::MAX as usize {
                        std::process::abort();
                    }

                    // If nobody is holding a write lock or attempting to acquire it, increment the
                    // number of readers.
                    match self.state.compare_exchange(
                        state,
                        state + ONE_READER,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Poll::Ready(ReadGuard(self)),
                        Err(s) => state = s,
                    }
                } else {
                    // Start listening for "no writer" events.
                    let load_ordering = if let Some(l) = listener.as_mut() {
                        // Wait for the writer to finish.
                        ready!(Pin::new(l).poll(cx));
                        listener = None;

                        // Notify the next reader waiting in list.
                        self.no_writer.notify(1);

                        // Check the state again.
                        Ordering::Acquire
                    } else {
                        listener = Some(self.no_writer.listen());

                        // Make sure there really is no writer.
                        Ordering::SeqCst
                    };

                    // Reload the state.
                    state = self.state.load(load_ordering);
                }
            }
        })
        .await
    }

    async fn write(&self) -> WriteGuard<'_> {
        let mut lock = Some(Box::pin(self.mutex.lock()));
        let mut listener = None;
        let mut guard = None;
        let mut release = None;

        future::poll_fn(move |cx| {
            loop {
                match lock.as_mut() {
                    Some(l) => {
                        // First grab the mutex.
                        let mutex_guard = ready!(l.as_mut().poll(cx));
                        lock = None;

                        // Set `WRITER_BIT` and create a guard that unsets it in case this future is canceled.
                        let new_state = self.state.fetch_or(WRITER_BIT, Ordering::SeqCst);

                        // If we just acquired the lock, return.
                        if new_state == WRITER_BIT {
                            return Poll::Ready(WriteGuard {
                                lock: self,
                                _guard: mutex_guard,
                            });
                        }

                        // Start waiting for the readers to finish.
                        listener = Some(self.no_readers.listen());
                        guard = Some(mutex_guard);
                        release = Some(CallOnDrop(|| {
                            self.write_unlock();
                        }));
                    }

                    None => {
                        let load_ordering = if listener.is_none() {
                            Ordering::Acquire
                        } else {
                            Ordering::SeqCst
                        };

                        // Check the state again.
                        if self.state.load(load_ordering) == WRITER_BIT {
                            // We are the only ones holding the lock, return `Ready`.
                            std::mem::forget(release.take());
                            return Poll::Ready(WriteGuard {
                                lock: self,
                                _guard: guard.take().unwrap(),
                            });
                        }

                        // Wait for the readers to finish.
                        if let Some(l) = listener.as_mut() {
                            ready!(Pin::new(l).poll(cx));
                            listener = None;
                        } else {
                            listener = Some(self.no_readers.listen());
                        };
                    }
                }
            }
        })
        .await
    }

    fn write_unlock(&self) {
        // Unset `WRITER_BIT`.
        self.state.fetch_and(!WRITER_BIT, Ordering::SeqCst);
        // Trigger the "no writer" event.
        self.no_writer.notify(1);
    }
}

/// RAII read guard for RwLock.
struct ReadGuard<'a>(&'a RwLock);

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // Decrement the number of readers.
        if self.0.state.fetch_sub(ONE_READER, Ordering::SeqCst) & !WRITER_BIT == ONE_READER {
            // If this was the last reader, trigger the "no readers" event.
            self.0.no_readers.notify(1);
        }
    }
}

/// RAII write guard for RwLock.
struct WriteGuard<'a> {
    lock: &'a RwLock,
    _guard: MutexGuard<'a>,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.lock.write_unlock();
    }
}

#[test]
fn smoke() {
    future::block_on(async {
        let lock = RwLock::new();
        drop(lock.read().await);
        drop(lock.write().await);
        drop((lock.read().await, lock.read().await));
        drop(lock.write().await);
    });
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn contention() {
    const N: u32 = 10;
    const M: usize = if cfg!(miri) { 100 } else { 1000 };

    let (tx, rx) = flume::unbounded();
    let tx = Arc::new(tx);
    let rw = Arc::new(RwLock::new());

    // Spawn N tasks that randomly acquire the lock M times.
    for _ in 0..N {
        let tx = tx.clone();
        let rw = rw.clone();

        spawn(async move {
            for _ in 0..M {
                if fastrand::u32(..N) == 0 {
                    drop(rw.write().await);
                } else {
                    drop(rw.read().await);
                }
            }
            tx.send_async(()).await.unwrap();
        });
    }

    future::block_on(async move {
        for _ in 0..N {
            rx.recv_async().await.unwrap();
        }
    });
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn writer_and_readers() {
    let lock = Arc::new(RwLock::new());
    let (tx, rx) = flume::unbounded();

    // Spawn a writer task.
    spawn({
        let lock = lock.clone();
        async move {
            let _lock = lock.write().await;
            for _ in 0..1000 {
                future::yield_now().await;
            }
            tx.send_async(()).await.unwrap();
        }
    });

    // Readers try to catch the writer in the act.
    let mut readers = Vec::new();
    for _ in 0..5 {
        let lock = lock.clone();
        readers.push(spawn(async move {
            for _ in 0..1000 {
                let lock = lock.read().await;
                drop(lock);
            }
        }));
    }

    future::block_on(async move {
        // Wait for readers to pass their asserts.
        for r in readers {
            r.await;
        }

        // Wait for writer to finish.
        rx.recv_async().await.unwrap();
        let lock = lock.read().await;
        drop(lock);
    });
}
