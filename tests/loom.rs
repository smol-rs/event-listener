//! Loom tests.

#![cfg(all(feature = "loom", loom))]

use event_listener::Event;
use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::{Arc, Condvar, Mutex};

struct EventWithCondvar {
    event: Event,
    waiter_count: Mutex<usize>,
    condvar: Condvar,
}

/// A simple Mutex built using `Event` and `loom` primitives.
struct EventMutex<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
    event: Event,
}

impl<T> EventMutex<T> {
    fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        loop {
            // Try to lock the mutex.
            if self
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // We have successfully locked the mutex.
                let result = self.data.with_mut(|data| unsafe { f(&mut *data) });
                self.locked.store(false, Ordering::Release);
                self.event.notify(1);
                return result;
            }

            // The mutex is locked. Wait for it to become unlocked.
            let listener = self.event.listen();

            // Check if the mutex is still locked.
            if self.locked.load(Ordering::Relaxed) {
                // The mutex is still locked. Wait for it to become unlocked.
                listener.wait();
            }
        }
    }
}

#[test]
fn two_listeners() {
    loom::model(|| {
        // Set up two threads to wait on the event.
        let event = Arc::new(EventWithCondvar {
            event: Event::new(),
            waiter_count: Mutex::new(0),
            condvar: Condvar::new(),
        });
        let mut handles = Vec::new();

        for _ in 0..2 {
            let event = event.clone();
            handles.push(loom::thread::spawn(move || {
                let listener = event.event.listen();

                // Update the waiter count.
                let mut waiter_count = event.waiter_count.lock().unwrap();
                *waiter_count += 1;
                drop(waiter_count);
                event.condvar.notify_all();

                listener.wait();
            }));
        }

        // Notify both threads once they're waiting.
        let mut waiter_count = event.waiter_count.lock().unwrap();
        loop {
            if *waiter_count == 2 {
                break;
            }

            waiter_count = event.condvar.wait(waiter_count).unwrap();
        }

        event.event.notify(2);

        // Wait for both threads to finish.
        for handle in handles {
            handle.join().unwrap();
            event.event.notify(1);
        }
    });
}

#[test]
fn mutex() {
    loom::model(|| {
        // Create a mutex.
        let mutex = Arc::new(EventMutex {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(0),
            event: Event::new(),
        });

        // Set up two threads to access the mutex.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let mutex = mutex.clone();
            handles.push(loom::thread::spawn(move || {
                mutex.with_lock(|data| {
                    *data += 1;
                });
            }));
        }

        // Wait for both threads to finish.
        for handle in handles {
            handle.join().unwrap();
        }

        // Check that the mutex was accessed exactly twice.
        mutex.with_lock(|data| {
            assert_eq!(*data, 2);
        });
    });
}
