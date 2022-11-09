//! The node that makes up queues.

use crate::inner::Inner;
use crate::list::{Entry, List, State};
use crate::{Notify, NotifyKind, Task};

use alloc::boxed::Box;
use core::ptr::NonNull;

/// A node in the backup queue.
pub(crate) enum Node {
    /// This node is requesting to add a listener.
    AddListener {
        /// The pointer to the listener to add.
        listener: Option<DistOwnedListener>,
    },

    /// This node is notifying a listener.
    Notify(Notify),

    /// This node is removing a listener.
    RemoveListener {
        /// The pointer to the listener to remove.
        listener: NonNull<Entry>,

        /// Whether to propagate notifications to the next listener.
        propagate: bool,
    },

    /// We are waiting for the mutex to lock, so they can manipulate it.
    Waiting(Task),
}

pub(crate) struct DistOwnedListener(NonNull<Entry>);

impl DistOwnedListener {
    /// extracts the contained entry pointer from the DOL,
    /// without calling the DOL Drop handler (such that the returned pointer stays valid)
    fn take(self) -> NonNull<Entry> {
        (&*core::mem::ManuallyDrop::new(self)).0
    }
}

impl Drop for DistOwnedListener {
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl Node {
    pub(crate) fn listener() -> (Self, NonNull<Entry>) {
        let entry = Box::into_raw(Box::new(Entry::new()));
        let entry = unsafe { NonNull::new_unchecked(entry) };
        (Self::AddListener { listener: Some(DistOwnedListener(entry)) }, entry)
    }

    /// Indicate that this node has been enqueued.
    pub(crate) fn enqueue(&self) {
        if let Node::AddListener { listener: Some(entry) } = self {
            unsafe { entry.0.as_ref() }.enqueue();
        }
    }

    /// Apply the node to the list.
    pub(crate) fn apply(self, list: &mut List, inner: &Inner) -> Option<Task> {
        match self {
            Node::AddListener { mut listener } => {
                // Add the listener to the list.
                let entry = listener.take().unwrap().take();
                list.insert(entry);

                // Dequeue the listener.
                return unsafe { entry.as_ref().dequeue() };
            }
            Node::Notify(Notify { count, kind }) => {
                // Notify the listener.
                match kind {
                    NotifyKind::Notify => list.notify_unnotified(count),
                    NotifyKind::NotifyAdditional => list.notify_additional(count),
                }
            }
            Node::RemoveListener {
                listener,
                propagate,
            } => {
                // Remove the listener from the list.
                let state = list.remove(listener, inner.cache_ptr());

                if let (true, State::Notified(additional)) = (propagate, state) {
                    // Propagate the notification to the next listener.
                    list.notify(1, additional);
                }
            }
            Node::Waiting(task) => {
                return Some(task);
            }
        }

        None
    }
}
