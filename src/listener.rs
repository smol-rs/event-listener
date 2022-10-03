//! Implementation of the inner listener primitive.
//!
//! This is stored in a `ConcurrentQueue` inside of an event and in the `EventListener` handle.
//! This means that the maximum refcount of the event is 2. In addition, it will either be
//! allocated to the heap or to a buffer inside of the `Event` itself.
//!
//! This module aims to create a primitive that fulfills all of these requirements while being
//! safe to use from the main module.

use super::Inner;

use alloc::boxed::Box;

use core::cell::UnsafeCell as StdUnsafeCell;
use core::mem::{forget, MaybeUninit};
use core::ptr::{self, NonNull};
use core::task::Waker;

#[cfg(feature = "std")]
use std::panic::{RefUnwindSafe, UnwindSafe};

use crate::busy_wait;
use crate::sync::atomic::Ordering::{AcqRel, Acquire, Release, SeqCst};
use crate::sync::atomic::{fence, AtomicBool, AtomicUsize};
use crate::sync::UnsafeCell;

#[cfg(not(loom))]
use crate::sync::AtomicWithMut;

#[cfg(feature = "std")]
use crate::sync::Unparker;

/// The internal listener for the `Event`.
pub(crate) struct Listener {
    /// The current state of the listener.
    state: AtomicUsize,

    /// The task that this listener is blocked on.
    task: UnsafeCell<MaybeUninit<Task>>,
}

/// A cached `Listener` on the stack.
pub(crate) struct CachedListener {
    /// Whether or not the listener is cached.
    cached: AtomicBool,

    /// The cached listener.
    listener: StdUnsafeCell<MaybeUninit<Listener>>,
}

unsafe impl Send for Listener {}
unsafe impl Sync for Listener {}

#[cfg(feature = "std")]
impl UnwindSafe for Listener {}
#[cfg(feature = "std")]
impl RefUnwindSafe for Listener {}

impl Default for Listener {
    fn default() -> Self {
        // Assume that listeners start off as queued.
        Self {
            state: AtomicUsize::new(ListenerStatus::Created as usize | QUEUED_MASK),
            task: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

impl Default for CachedListener {
    fn default() -> Self {
        Self {
            cached: AtomicBool::new(false),
            listener: MaybeUninit::uninit().into(),
        }
    }
}

impl Listener {
    /// Allocate a new `Listener`.
    pub(crate) fn alloc(event: &Inner) -> NonNull<Listener> {
        // If the cache is open, then we don't have to allocate.
        if !event.cached.cached.swap(true, Acquire) {
            // SAFETY: The cache is open and the queue is empty, so we know that the cache is
            // valid to write to.
            let ptr = event.cached.listener.get();
            unsafe {
                ptr.write(MaybeUninit::new(Listener::default()));
                return NonNull::new_unchecked(ptr as *mut Listener);
            }
        }

        // The cache is unavailable, so we have to allocate.
        let listener = Box::new(Listener::default());
        let ptr = Box::into_raw(listener);

        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// Begin waiting on this `Listener`.
    ///
    /// Returns `true` if the listener was notified.
    pub(crate) fn wait(this: NonNull<Self>, task: impl FnOnce() -> Task) -> bool {
        let this = unsafe { this.as_ref() };
        let mut state: State = this.state.load(Acquire).into();

        loop {
            match state.status {
                ListenerStatus::Created | ListenerStatus::Task => {
                    // Try to "lock" the listener.
                    let writing_state = State {
                        status: ListenerStatus::WritingTask,
                        ..state
                    };
                    if let Err(e) = this.state.compare_exchange(
                        state.into(),
                        writing_state.into(),
                        SeqCst,
                        SeqCst,
                    ) {
                        state = e.into();
                        busy_wait();
                        continue;
                    }

                    // We now hold the "lock" on the task slot. Write the task to the slot.
                    let guard = ResetState(&this.state, state);
                    let task = task();
                    forget(guard);

                    let task = this.task.with_mut(|slot| unsafe {
                        // If there already was a task, swap it out and wake it.
                        if state.status == ListenerStatus::Task {
                            Some(ptr::replace(slot.cast(), task))
                        } else {
                            ptr::write(slot.cast(), task);
                            None
                        }
                    });

                    // We are done writing to the task slot. Transition to `Task`.
                    // No other thread can transition to `Task` from `WritingTask`.
                    state.status = ListenerStatus::Task;
                    this.state.store(state.into(), Release);

                    // If we yielded a task, wake it now.
                    if let Some(task) = task {
                        task.wake();
                    }

                    // Now, we should wait for a notification.
                    return false;
                }
                ListenerStatus::WritingTask => {
                    // We must be in the process of being woken up. Wait for the task to be written.
                    busy_wait();
                }
                ListenerStatus::Notified | ListenerStatus::NotifiedAdditional => {
                    // We were already notified. We are done.
                    return true;
                }
                ListenerStatus::Orphaned => {
                    // The event was dropped. We are done.
                    return false;
                }
            }

            /// The task() closure may clone a user-defined waker, which can panic.
            ///
            /// This panic would leave the listener in the `WritingTask` state, which will
            /// lead to an infinite loop. This guard resets it back to the original state.
            struct ResetState<'a>(&'a AtomicUsize, State);

            impl Drop for ResetState<'_> {
                fn drop(&mut self) {
                    self.0.store(self.1.into(), Release);
                }
            }
        }
    }

    /// Notify this `Listener`.
    ///
    /// Returns `true` if the listener was successfully notified. This implicity deques the listener and
    /// may destroy this listener if the top-level EventListener has already orphaned it.
    pub(crate) fn notify(entry: NonNull<Listener>, additional: bool, event: &Inner) -> bool {
        let this = unsafe { entry.as_ref() };

        let new_state = if additional {
            ListenerStatus::NotifiedAdditional
        } else {
            ListenerStatus::Notified
        };
        let new_state = State {
            status: new_state,
            queued: false,
        };

        let mut state: State = this.state.load(Acquire).into();

        let notified = loop {
            // Determine what state we're in.
            match state.status {
                ListenerStatus::Created
                | ListenerStatus::Notified
                | ListenerStatus::NotifiedAdditional => {
                    // Indicate that the listener was notified and is now unqueued.
                    if let Err(e) =
                        this.state
                            .compare_exchange(state.into(), new_state.into(), AcqRel, Acquire)
                    {
                        // Someone got to it before we did, try again.
                        state = e.into();
                        continue;
                    } else {
                        // We successfully notified the listener.
                        break true;
                    }
                }
                ListenerStatus::WritingTask => {
                    // The listener is currently writing the task, wait until they finish.
                }
                ListenerStatus::Task => {
                    // We need to wake the listener before we can do anything else.
                    let writing_state = State {
                        queued: state.queued,
                        status: ListenerStatus::WritingTask,
                    };

                    if let Err(e) = this.state.compare_exchange(
                        state.into(),
                        writing_state.into(),
                        AcqRel,
                        Acquire,
                    ) {
                        // Someone else got to it before we did.
                        state = e.into();
                        busy_wait();
                        continue;
                    }

                    // SAFETY: Since we hold the lock, we can now read out the primitive.
                    let task = this
                        .task
                        .with_mut(|task| unsafe { ptr::read(task.cast::<Task>()) });

                    // SAFETY: No other code makes a change when `WritingTask` is detected.
                    this.state.store(new_state.into(), Release);

                    // Wake the task up and return.
                    task.wake();
                    break true;
                }
                ListenerStatus::Orphaned => {
                    // This task is no longer being monitored.
                    break false;
                }
            }
        };

        // If the listener orphaned this listener, we need to destroy it.
        if let ListenerStatus::Orphaned = state.status {
            // Create a memory fence and then destroy it.
            fence(Acquire);
            Self::destroy(entry, event);
        }

        notified
    }

    /// Orphan this `Listener`.
    ///
    /// Returns `Some<bool>` if a notification was discarded. The `bool` is `true`
    /// if the notification was an additional notification. If this entry was already
    /// removed from the queue, this destroys the entry as well.
    pub(crate) fn orphan(entry: NonNull<Listener>, event: &Inner) -> Option<bool> {
        let this = unsafe { entry.as_ref() };
        let mut state: State = this.state.load(Acquire).into();

        let result = loop {
            match state.status {
                ListenerStatus::Created
                | ListenerStatus::Notified
                | ListenerStatus::NotifiedAdditional => {
                    let new_state = State {
                        status: ListenerStatus::Orphaned,
                        queued: state.queued,
                    };

                    // Indicate that the listener was notified.
                    if let Err(new_state) =
                        this.state
                            .compare_exchange(state.into(), new_state.into(), AcqRel, Acquire)
                    {
                        // We failed to transition to `Orphaned`. Try again.
                        state = new_state.into();
                        busy_wait();
                        continue;
                    }

                    match state.status {
                        ListenerStatus::Notified => break Some(false),
                        ListenerStatus::NotifiedAdditional => break Some(true),
                        _ => break None,
                    }
                }
                ListenerStatus::WritingTask => {
                    // We're in the middle of being notified, wait for them to finish writing first.
                }
                ListenerStatus::Task => {
                    let writing_state = State {
                        queued: state.queued,
                        status: ListenerStatus::WritingTask,
                    };

                    if let Err(new_state) = this.state.compare_exchange(
                        state.into(),
                        writing_state.into(),
                        AcqRel,
                        Acquire,
                    ) {
                        // Someone else got to it before we did.
                        state = new_state.into();
                        busy_wait();
                        continue;
                    }

                    // SAFETY: Since we hold the lock, we can now drop the primitive.
                    this.task
                        .with_mut(|task| unsafe { ptr::drop_in_place(task.cast::<Task>()) });

                    // SAFETY: No other code makes a change when `WritingTask` is detected.
                    state.status = ListenerStatus::Orphaned;
                    this.state.store(state.into(), Release);

                    // We did not discard a notification.
                    break None;
                }
                ListenerStatus::Orphaned => {
                    // We have already been orphaned.
                    break None;
                }
            }

            busy_wait();
            state = this.state.load(Acquire).into();
        };

        // If the entry is dequeued, we need to destroy it.
        if !state.queued {
            fence(Acquire);
            Self::destroy(entry, event);
        }

        // At this point, if we are no longer queued, we can drop the listener.
        result
    }

    fn destroy(entry: NonNull<Listener>, event: &Inner) {
        // If this pointer is equal to the cache pointer, we can just clear the cache.
        let cache_ptr = event.cached.listener.get().cast();
        if entry.as_ptr() == cache_ptr {
            unsafe {
                ptr::drop_in_place(cache_ptr);
            }

            // Mark the cache as empty.
            event.cached.cached.store(false, Release);

            return;
        }

        drop(unsafe { Box::from_raw(entry.as_ptr()) });
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let Self {
            ref mut state,
            ref mut task,
        } = self;

        state.with_mut(|state| {
            if let ListenerStatus::Task = State::from(*state).status {
                // We're still holding onto a task, drop it.
                task.with_mut(|task| unsafe { ptr::drop_in_place(task.cast::<Task>()) });
            }
        });
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct State {
    /// The status associated with the top-level EventListener.
    status: ListenerStatus,

    /// The status associated with the queue.
    queued: bool,
}

/// The state that a `Listener` can be in.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(usize)]
enum ListenerStatus {
    /// The listener was just created.
    Created,

    /// The listener has been notified through `notify()`.
    Notified,

    /// The listener has been notified through `notify_additional()`.
    NotifiedAdditional,

    /// The listener is being used to hold a task.
    ///
    /// If the listener is in this state, then the `task` field is guaranteed to
    /// be initialized.
    Task,

    /// The `task` field is being written to.
    WritingTask,

    /// The listener is being dropped.
    Orphaned,
}

const LISTENER_STATUS_MASK: usize = 0b1111;
const QUEUED_SHIFT: usize = 4;
const QUEUED_MASK: usize = 0b1 << QUEUED_SHIFT;

impl From<usize> for State {
    fn from(val: usize) -> Self {
        let status = match val & LISTENER_STATUS_MASK {
            0 => ListenerStatus::Created,
            1 => ListenerStatus::Notified,
            2 => ListenerStatus::NotifiedAdditional,
            3 => ListenerStatus::Task,
            4 => ListenerStatus::WritingTask,
            5 => ListenerStatus::Orphaned,
            _ => unreachable!("invalid state"),
        };

        let queued = (val & QUEUED_MASK) != 0;

        Self { status, queued }
    }
}

impl From<State> for usize {
    fn from(state: State) -> Self {
        let status = state.status as usize;
        let queued = if state.queued { QUEUED_MASK } else { 0 };

        status | queued
    }
}

/// The task to wake up once a notification is received.
pub(crate) enum Task {
    /// The task is an async task waiting on a `Waker`.
    Waker(Waker),

    /// The task is a thread blocked on the `Unparker`.
    #[cfg(feature = "std")]
    Thread(Unparker),
}

impl Task {
    fn wake(self) {
        match self {
            Self::Waker(waker) => waker.wake(),
            #[cfg(feature = "std")]
            Self::Thread(unparker) => {
                unparker.unpark();
            }
        }
    }
}
