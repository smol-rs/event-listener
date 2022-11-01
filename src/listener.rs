//! A listener in the queue of listeners.

use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::cell::Cell;

use core::task::Waker;

#[cfg(feature = "std")]
use parking::Unparker;

/// A listener in the queue of listeners.
pub(crate) struct Listener {
    /// The current state of this listener.
    ///
    /// This also serves as the refcount:
    /// - Ref 1: The listener state is not orphaned.
    /// - Ref 2: The listener is in the queue (queue bit is set).
    state: AtomicUsize,

    /// The waker or thread handle that will be notified when the listener is
    /// ready.
    ///
    /// This is kept synchronized by the `state` variable; therefore, it's
    /// technically `Sync`.
    waker: Cell<Wakeup>,
}

impl Listener {
    /// Create a new listener.
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(
                State {
                    listen: ListenState::Created,
                    queued: false,
                }
                .into(),
            ),
            waker: Cell::new(Wakeup::None),
        }
    }

    /// Enqueue this listener.
    pub(crate) fn enqueue(&self) {
        let mut state = State::from(self.state.load(Ordering::Acquire));

        // If the listener is already queued, then we don't need to do anything.
        loop {
            if state.queued {
                return;
            }

            // Mark the listener as queued.
            let new_state = State {
                queued: true,
                ..state
            };

            match self.state.compare_exchange(
                state.into(),
                new_state.into(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => state = State::from(actual),
            }
        }
    }

    /// Dequeue this listener.
    ///
    /// Returns `true` if the listener is also orphaned, and that the caller
    /// should drop the listener.
    pub(crate) fn dequeue(&self) -> bool {
        let mut state = State::from(self.state.load(Ordering::Acquire));

        // If the listener is not queued, then we don't need to do anything.
        loop {
            if !state.queued {
                return false;
            }

            // Mark the listener as not queued.
            let new_state = State {
                queued: false,
                ..state
            };

            match self.state.compare_exchange(
                state.into(),
                new_state.into(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => state = State::from(actual),
            }
        }

        // If the listener is orphaned, then we need to drop it.
        state.listen == ListenState::Orphaned
    }

    /// Orphan this listener, and return `true` if we need to be dropped.
    ///
    /// This method is called when `EventListener` is dropped. The second result is whether or not
    /// the listener was notified (and `true` if the notification was additional.)
    pub(crate) fn orphan(&self) -> (bool, Option<bool>) {
        let mut state = State::from(self.state.load(Ordering::Acquire));

        loop {
            match state.listen {
                ListenState::Orphaned => {
                    // If the listener is already orphaned, then we don't need to do
                    // anything.
                    return (false, None);
                }
                ListenState::SettingWaker => {
                    // We may be in the middle of being notified. Wait for the
                    // notification to complete.
                    crate::yield_now();
                    state = State::from(self.state.load(Ordering::Acquire));
                    continue;
                }
                _ => {}
            }

            // Mark the listener as orphaned.
            let new_state = State {
                listen: ListenState::Orphaned,
                ..state
            };

            match self.state.compare_exchange(
                state.into(),
                new_state.into(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    break;
                }
                Err(actual) => state = State::from(actual),
            }
        }

        // Determine if we were woken before we were orphaned.
        let notified = match state.listen {
            ListenState::Notified => Some(false),
            ListenState::NotifiedAdditional => Some(true),
            ListenState::Registered => {
                // Make sure to delete the waker.
                self.waker.replace(Wakeup::None);

                None
            }
            _ => None,
        };

        // If the listener is not queued, then we need to drop it.
        (!state.queued, notified)
    }

    /// Check to see if we have been notified yet.
    ///
    /// Returns true if we have been notified, and false otherwise.
    pub(crate) fn register(&self, init: impl FnOnce() -> Wakeup) -> bool {
        let mut state = State::from(self.state.load(Ordering::Acquire));

        loop {
            match state.listen {
                ListenState::Orphaned => {
                    // We've been orphaned, so there is no way we will be notified.
                    return false;
                }
                ListenState::Created | ListenState::Registered => {
                    // We are not yet notified, so we need to register the waker.
                    let new_state = State {
                        listen: ListenState::SettingWaker,
                        ..state
                    };

                    match self.state.compare_exchange(
                        state.into(),
                        new_state.into(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            // We're the first to register, so we need to set the waker.
                            self.waker.set(init());

                            // Mark the listener as registered.
                            let new_state = State {
                                listen: ListenState::Registered,
                                ..state
                            };

                            self.state.store(new_state.into(), Ordering::Release);

                            return false;
                        }
                        Err(actual) => {
                            state = State::from(actual);
                            continue;
                        }
                    }
                }
                ListenState::Notified | ListenState::NotifiedAdditional => {
                    // We've been notified!
                    return true;
                }
                ListenState::SettingWaker => {
                    // We may be in the middle of being notified. Wait for the
                    // notification to complete.
                    crate::yield_now();
                    state = State::from(self.state.load(Ordering::Acquire));
                    continue;
                }
            }
        }
    }

    /// Notify this listener.
    ///
    /// Returns "true" if the notification did anything.
    pub(crate) fn notify(&self, additional: bool) -> bool {
        let mut state = State::from(self.state.load(Ordering::Acquire));
        let new_listen = if additional {
            ListenState::NotifiedAdditional
        } else {
            ListenState::Notified
        };

        loop {
            // Determine what we want the new state to be.
            let new_state = State {
                listen: new_listen,
                ..state
            };

            match state.listen {
                ListenState::Notified | ListenState::NotifiedAdditional | ListenState::Orphaned => {
                    // We're already either notified or orphaned.
                    return false;
                }
                ListenState::Created => {
                    // They haven't registered a wakeup yet, so register them.
                    if let Err(actual) = self.state.compare_exchange(
                        state.into(),
                        new_state.into(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        // We failed to register them, so we need to try again.
                        crate::yield_now();
                        state = State::from(actual);
                        continue;
                    }

                    return true;
                }
                ListenState::Registered => {
                    let intermediate_state = State {
                        listen: ListenState::SettingWaker,
                        ..state
                    };

                    // We're registered, so we need to wake them up.
                    if let Err(actual) = self.state.compare_exchange(
                        state.into(),
                        intermediate_state.into(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        // We failed to lock the waker, try again.
                        crate::yield_now();
                        state = State::from(actual);
                        continue;
                    }

                    // If a waker panics, set the state to notified anyways.
                    let _cleanup = Cleanup(|| {
                        self.state.store(new_state.into(), Ordering::Release);
                    });

                    // Wake them up.
                    self.waker.replace(Wakeup::None).wake();

                    return true;
                }
                ListenState::SettingWaker => {
                    // The listener is setting the waker, so we need to wait for them to finish.
                    crate::yield_now();
                    state = State::from(self.state.load(Ordering::Acquire));
                    continue;
                }
            }
        }
    }
}

/// The current state of a listener.
///
/// This is stored in a `usize` in order to allow it to be used atomically.
#[derive(Copy, Clone)]
struct State {
    /// The current listener state.
    listen: ListenState,

    /// Whether or not we are queued.
    queued: bool,
}

const LISTEN_STATE_MASK: usize = 0b111;
const QUEUED_BIT: usize = 0b1000;

impl From<usize> for State {
    fn from(value: usize) -> Self {
        // Determine the `ListenState`.
        let listen = match value & LISTEN_STATE_MASK {
            0 => ListenState::Created,
            1 => ListenState::Registered,
            2 => ListenState::SettingWaker,
            3 => ListenState::Notified,
            4 => ListenState::NotifiedAdditional,
            5 => ListenState::Orphaned,
            _ => unreachable!("invalid state"),
        };

        // Determine if we are queued.
        let queued = (value & QUEUED_BIT) != 0;

        Self { listen, queued }
    }
}

impl From<State> for usize {
    fn from(state: State) -> Self {
        state.listen as usize | (if state.queued { QUEUED_BIT } else { 0 })
    }
}

/// The current state of a listener.
///
/// This is the portion that matters to the `EventListener` structure.
#[repr(usize)]
#[derive(Copy, Clone, PartialEq, Eq)]
enum ListenState {
    /// We've just been created and are not yet registered.
    Created = 0,

    /// We've been registered with a task.
    ///
    /// The `waker` field is initialized when we transition to this state.
    Registered = 1,

    /// We are currently editing the `waker` field.
    ///
    /// When we are in this state, someone is modifying the `waker` field.
    /// We shouldn't do anything until they're done.
    SettingWaker = 2,

    /// We have been notified, using the `notify()` function.
    Notified = 3,

    /// We have been notified, using the `notify_additional()` function.
    NotifiedAdditional = 4,

    /// The event listener is no longer paying attention to us.
    ///
    /// The `EventListener` reference no longer exists. If we are in this state,
    /// and the `QUEUED` bit is no longer set, we should drop ourselves.
    Orphaned = 5,
}

/// A waker or thread handle that will be notified when the listener is ready.
pub(crate) enum Wakeup {
    /// No waker or thread handle has been set.
    None,

    /// A waker has been set.
    Waker(Waker),

    /// A thread handle has been set.
    #[cfg(feature = "std")]
    Thread(Unparker),
}

impl Wakeup {
    fn wake(self) {
        match self {
            Wakeup::None => {}
            Wakeup::Waker(waker) => waker.wake(),
            #[cfg(feature = "std")]
            Wakeup::Thread(unparker) => {
                unparker.unpark();
            }
        }
    }
}

struct Cleanup<F: Fn()>(F);

impl<F: Fn()> Drop for Cleanup<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
