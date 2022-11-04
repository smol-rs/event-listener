//! The node that makes up queues.

use crate::inner::Inner;
use crate::list::{Entry, List, State};
use crate::sync::atomic::AtomicPtr;
use crate::{Notify, NotifyKind, Task};

use alloc::boxed::Box;
use core::ptr::{self, NonNull};

/// A node in the backup queue.
pub(crate) struct Node {
    /// The next node in the queue.
    next: AtomicPtr<Node>,

    /// The data associated with the node.
    data: NodeData,
}

enum NodeData {
    /// This node is requesting to add a listener.
    AddListener {
        /// The pointer to the listener to add.
        listener: NonNull<Entry>,
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

impl Node {
    /// Create a new listener submission entry.
    pub(crate) fn listener() -> (Self, NonNull<Entry>) {
        // Allocate an entry on the heap.
        let entry = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(Entry::new()))) };

        (
            Self {
                next: AtomicPtr::new(ptr::null_mut()),
                data: NodeData::AddListener { listener: entry },
            },
            entry,
        )
    }

    pub(crate) fn next(&self) -> &AtomicPtr<Node> {
        &self.next
    }

    /// Create a new notification entry.
    pub(crate) fn notify(notify: Notify) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            data: NodeData::Notify(notify),
        }
    }

    /// Create a new listener removal entry.
    pub(crate) fn remove_listener(listener: NonNull<Entry>, propagate: bool) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            data: NodeData::RemoveListener {
                listener,
                propagate,
            },
        }
    }

    /// Create a new waiting entry.
    pub(crate) fn waiting(task: Task) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            data: NodeData::Waiting(task),
        }
    }

    /// Indicate that this node has been enqueued.
    pub(crate) fn enqueue(&self) {
        if let NodeData::AddListener { listener } = &self.data {
            unsafe {
                listener.as_ref().enqueue();
            }
        }
    }

    /// Apply the node to the list.
    pub(crate) fn apply(self, list: &mut List, inner: &Inner) -> Option<Task> {
        match self.data {
            NodeData::AddListener { listener } => {
                // Add the listener to the list.
                list.insert(listener);

                // Dequeue the listener.
                return unsafe { listener.as_ref().dequeue() };
            }
            NodeData::Notify(notify) => {
                // Notify the listener.
                let Notify { count, kind } = notify;

                match kind {
                    NotifyKind::Notify => list.notify(count),
                    NotifyKind::NotifyAdditional => list.notify_additional(count),
                }
            }
            NodeData::RemoveListener {
                listener,
                propagate,
            } => {
                // Remove the listener from the list.
                let state = list.remove(listener);

                if let (true, State::Notified(additional)) = (propagate, state) {
                    // Propagate the notification to the next listener.
                    if additional {
                        list.notify_additional(1);
                    } else {
                        list.notify(1);
                    }
                } else if !propagate {
                    // Just delete the listener.
                    unsafe {
                        list.dealloc(listener, inner.cache_ptr());
                    }
                }
            }
            NodeData::Waiting(task) => return Some(task),
        }

        None
    }
}
