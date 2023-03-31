//! The node that makes up queues.

use crate::list::{Listener, ListenerSlab};
use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::Arc;
use crate::{State, Task};

use core::num::NonZeroUsize;
use crossbeam_utils::atomic::AtomicCell;

/// A node in the backup queue.
pub(crate) enum Node {
    /// This node is requesting to add a listener.
    // For some reason, the MSRV build says this variant is never constructed.
    #[allow(dead_code)]
    AddListener {
        /// The state of the listener that wants to be added.
        task_waiting: Arc<TaskWaiting>,
    },

    /// This node is notifying a listener.
    Notify {
        /// The number of listeners to notify.
        count: usize,

        /// Whether to wake up notified listeners.
        additional: bool,
    },

    /// This node is removing a listener.
    RemoveListener {
        /// The ID of the listener to remove.
        listener: Listener,

        /// Whether to propagate notifications to the next listener.
        propagate: bool,
    },

    /// We are waiting for the mutex to lock, so they can manipulate it.
    Waiting(Task),
}

pub(crate) struct TaskWaiting {
    /// The task that is being waited on.
    task: AtomicCell<Option<Task>>,

    /// The ID of the new entry.
    ///
    /// This is set to zero when the task is still queued.
    entry_id: AtomicUsize,
}

impl Node {
    pub(crate) fn listener() -> (Self, Arc<TaskWaiting>) {
        // Create a new `TaskWaiting` structure.
        let task_waiting = Arc::new(TaskWaiting {
            task: AtomicCell::new(None),
            entry_id: AtomicUsize::new(0),
        });

        (
            Self::AddListener {
                task_waiting: task_waiting.clone(),
            },
            task_waiting,
        )
    }

    /// Apply the node to the list.
    pub(crate) fn apply(self, list: &mut ListenerSlab) -> Option<Task> {
        match self {
            Node::AddListener { task_waiting } => {
                // Add a new entry to the list.
                let key = list.insert(State::Created);

                // Send the new key to the listener and wake it if necessary.
                task_waiting.entry_id.store(key.get(), Ordering::Release);
                return task_waiting.task.take();
            }
            Node::Notify { count, additional } => {
                // Notify the listener.
                list.notify(count, additional);
            }
            Node::RemoveListener {
                listener,
                propagate,
            } => {
                // Remove the listener from the list.
                list.remove(listener, propagate);
            }
            Node::Waiting(task) => {
                return Some(task);
            }
        }

        None
    }
}

impl TaskWaiting {
    /// Determine if we are still queued.
    ///
    /// Returns `Some` with the entry ID if we are no longer queued.
    pub(crate) fn status(&self) -> Option<NonZeroUsize> {
        NonZeroUsize::new(self.entry_id.load(Ordering::Acquire))
    }

    /// Register a listener.
    pub(crate) fn register(&self, task: Task) {
        // Set the task.
        if let Some(task) = self.task.swap(Some(task)) {
            task.wake();
        }

        // If the entry ID is non-zero, then we are no longer queued.
        if self.status().is_some() {
            // Wake the task.
            self.task.take().unwrap().wake();
        }
    }
}
