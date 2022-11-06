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
    data: Option<NodeData>,
}

impl From<NodeData> for Node {
    fn from(data: NodeData) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            data: Some(data),
        }
    }
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

        (NodeData::AddListener { listener: entry }.into(), entry)
    } 

    /// Create a new notification entry.
    pub(crate) fn notify(notify: Notify) -> Self {
        NodeData::Notify(notify).into()
    }

    /// Create a new listener removal entry.
    pub(crate) fn remove_listener(listener: NonNull<Entry>, propagate: bool) -> Self {
        NodeData::RemoveListener {
            listener,
            propagate,
        }
        .into()
    }

    /// Create a new waiting entry.
    pub(crate) fn waiting(task: Task) -> Self {
        NodeData::Waiting(task).into()
    }

    pub(crate) fn next(&self) -> &AtomicPtr<Node> {
        &self.next
    }

    /// Indicate that this node has been enqueued.
    pub(crate) fn enqueue(&self) {
        if let Some(NodeData::AddListener { listener }) = &self.data {
            unsafe {
                listener.as_ref().enqueue();
            }
        }
    }

    /// Apply the node to the list.
    pub(crate) fn apply(mut self, list: &mut List, inner: &Inner) -> Option<Task> {
        let data = self.data.take().unwrap();

        match data {
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
                let state = list.remove(listener, inner.cache_ptr());

                if let (true, State::Notified(additional)) = (propagate, state) {
                    // Propagate the notification to the next listener.
                    if additional {
                        list.notify_additional(1);
                    } else {
                        list.notify(1);
                    }
                }
            }
            NodeData::Waiting(task) => {
                return Some(task);
            }
        }

        None
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(NodeData::AddListener { listener }) = self.data.take() {
            unsafe {
                drop(Box::from_raw(listener.as_ptr()));
            }
        }
    }
}
