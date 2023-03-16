//! libstd-based implementation of `event-listener`.
//!
//! This implementation crates an intrusive linked list of listeners.

use crate::sync::atomic::Ordering;
use crate::sync::cell::{Cell, UnsafeCell};
use crate::sync::{Mutex, MutexGuard};
use crate::{State, TaskRef};

use core::marker::PhantomPinned;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;

pub(super) struct List(Mutex<Inner>);

struct Inner {
    /// The head of the linked list.
    head: Option<NonNull<Link>>,

    /// The tail of the linked list.
    tail: Option<NonNull<Link>>,

    /// The first unnotified listener.
    next: Option<NonNull<Link>>,

    /// Total number of listeners.
    len: usize,

    /// The number of notified listeners.
    notified: usize,
}

impl List {
    /// Create a new, empty event listener list.
    pub(super) fn new() -> Self {
        Self(Mutex::new(Inner {
            head: None,
            tail: None,
            next: None,
            len: 0,
            notified: 0,
        }))
    }
}

impl crate::Inner {
    fn lock(&self) -> ListLock<'_, '_> {
        ListLock {
            inner: self,
            lock: self.list.0.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// Add a new listener to the list.
    ///
    /// Does nothing is the listener is already registered.
    pub(crate) fn insert(&self, listener: Pin<&mut Option<Listener>>) {
        let mut inner = self.lock();

        // SAFETY: We are locked, so we can access the inner `link`.
        let entry = unsafe {
            // SAFETY: We never move out the `link` field.
            let listener = match listener.get_unchecked_mut() {
                listener @ None => {
                    // TODO: Use Option::insert once the MSRV is high enough.
                    *listener = Some(Listener {
                        link: UnsafeCell::new(Link {
                            state: Cell::new(State::Created),
                            prev: Cell::new(inner.tail),
                            next: Cell::new(None),
                        }),
                        _pin: PhantomPinned,
                    });

                    listener.as_mut().unwrap()
                }
                Some(_) => return,
            };

            // Get the inner pointer.
            &*listener.link.get()
        };

        // Replace the tail with the new entry.
        match mem::replace(&mut inner.tail, Some(entry.into())) {
            None => inner.head = Some(entry.into()),
            Some(t) => unsafe { t.as_ref().next.set(Some(entry.into())) },
        };

        // If there are no unnotified entries, this is the first one.
        if inner.next.is_none() {
            inner.next = inner.tail;
        }

        // Bump the entry count.
        inner.len += 1;
    }

    /// Remove a listener from the list.
    pub(crate) fn remove(
        &self,
        listener: Pin<&mut Option<Listener>>,
        propogate: bool,
    ) -> Option<State> {
        self.lock().remove(listener, propogate)
    }

    /// Notifies a number of entries.
    #[cold]
    pub(crate) fn notify(&self, n: usize, additional: bool) {
        self.lock().notify(n, additional)
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns `true` if the listener was already notified, and `false` otherwise. If the listener
    /// isn't inserted, returns `None`.
    pub(crate) fn register(
        &self,
        mut listener: Pin<&mut Option<Listener>>,
        task: TaskRef<'_>,
    ) -> Option<bool> {
        let mut inner = self.lock();

        // SAFETY: We are locked, so we can access the inner `link`.
        let entry = unsafe {
            // SAFETY: We never move out the `link` field.
            let listener = listener.as_mut().get_unchecked_mut().as_mut()?;
            &*listener.link.get()
        };

        // Take out the state and check it.
        match entry.state.replace(State::NotifiedTaken) {
            State::Notified(_) => {
                // We have been notified, remove the listener.
                inner.remove(listener, false);
                Some(true)
            }

            State::Task(other_task) => {
                // Only replace the task if it's different.
                if !task.will_wake(other_task.as_task_ref()) {
                    entry.state.set(State::Task(task.into_task()));
                } else {
                    entry.state.set(State::Task(other_task));
                }

                Some(false)
            }

            _ => {
                // We have not been notified, register the task.
                entry.state.set(State::Task(task.into_task()));
                Some(false)
            }
        }
    }
}

impl Inner {
    fn remove(
        &mut self,
        mut listener: Pin<&mut Option<Listener>>,
        propogate: bool,
    ) -> Option<State> {
        let entry = unsafe {
            // SAFETY: We never move out the `link` field.
            let listener = listener.as_mut().get_unchecked_mut().as_mut()?;

            // Get the inner pointer.
            &*listener.link.get()
        };

        let prev = entry.prev.get();
        let next = entry.next.get();

        // Unlink from the previous entry.
        match prev {
            None => self.head = next,
            Some(p) => unsafe {
                p.as_ref().next.set(next);
            },
        }

        // Unlink from the next entry.
        match next {
            None => self.tail = prev,
            Some(n) => unsafe {
                n.as_ref().prev.set(prev);
            },
        }

        // If this was the first unnotified entry, update the next pointer.
        if self.next == Some(entry.into()) {
            self.next = next;
        }

        // The entry is now fully unlinked, so we can now take it out safely.
        let entry = unsafe {
            listener
                .get_unchecked_mut()
                .take()
                .unwrap()
                .link
                .into_inner()
        };

        let state = entry.state.into_inner();

        // Update the notified count.
        if state.is_notified() {
            self.notified -= 1;

            if propogate {
                if let State::Notified(additional) = state {
                    self.notify(1, additional);
                }
            }
        }
        self.len -= 1;

        Some(state)
    }

    #[cold]
    fn notify(&mut self, mut n: usize, additional: bool) {
        if !additional {
            // Make sure we're not notifying more than we have.
            if n <= self.notified {
                return;
            }
            n -= self.notified;
        }

        while n > 0 {
            n -= 1;

            // Notify the next entry.
            match self.next {
                None => break,

                Some(e) => {
                    // Get the entry and move the pointer forwards.
                    let entry = unsafe { e.as_ref() };
                    self.next = entry.next.get();

                    // Set the state to `Notified` and notify.
                    if let State::Task(task) = entry.state.replace(State::Notified(additional)) {
                        task.wake();
                    }

                    // Bump the notified count.
                    self.notified += 1;
                }
            }
        }
    }
}

struct ListLock<'a, 'b> {
    lock: MutexGuard<'a, Inner>,
    inner: &'b crate::Inner,
}

impl Deref for ListLock<'_, '_> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.lock
    }
}

impl DerefMut for ListLock<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lock
    }
}

impl Drop for ListLock<'_, '_> {
    fn drop(&mut self) {
        let list = &mut **self;

        // Update the notified count.
        let notified = if list.notified < list.len {
            list.notified
        } else {
            core::usize::MAX
        };

        self.inner.notified.store(notified, Ordering::Release);
    }
}

pub(crate) struct Listener {
    /// The inner link in the linked list.
    ///
    /// # Safety
    ///
    /// This can only be accessed while the central mutex is locked.
    link: UnsafeCell<Link>,

    /// This listener cannot be moved after being pinned.
    _pin: PhantomPinned,
}

struct Link {
    /// The current state of the listener.
    state: Cell<State>,

    /// The previous link in the linked list.
    prev: Cell<Option<NonNull<Link>>>,

    /// The next link in the linked list.
    next: Cell<Option<NonNull<Link>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::pin;

    macro_rules! make_listeners {
        ($($id:ident),*) => {
            $(
                let $id = Option::<Listener>::None;
                pin!($id);
            )*
        };
    }

    #[test]
    fn insert() {
        let inner = crate::Inner::new();
        make_listeners!(listen1, listen2, listen3);

        // Register the listeners.
        inner.insert(listen1.as_mut());
        inner.insert(listen2.as_mut());
        inner.insert(listen3.as_mut());

        assert_eq!(inner.lock().len, 3);

        // Remove one.
        assert_eq!(inner.remove(listen2, false), Some(State::Created));
        assert_eq!(inner.lock().len, 2);

        // Remove another.
        assert_eq!(inner.remove(listen1, false), Some(State::Created));
        assert_eq!(inner.lock().len, 1);
    }

    #[test]
    fn drop_non_notified() {
        let inner = crate::Inner::new();
        make_listeners!(listen1, listen2, listen3);

        // Register the listeners.
        inner.insert(listen1.as_mut());
        inner.insert(listen2.as_mut());
        inner.insert(listen3.as_mut());

        // Notify one.
        inner.notify(1, false);

        // Remove one.
        inner.remove(listen3, true);

        // Remove the rest.
        inner.remove(listen1, true);
        inner.remove(listen2, true);
    }
}
