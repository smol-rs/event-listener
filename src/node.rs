//! The node that makes up queues.

use crate::list::{Entry, List, State};
use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::Arc;
use crate::{Notify, NotifyKind, Task};

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
    Notify(Notify),

    /// This node is removing a listener.
    RemoveListener {
        /// The ID of the listener to remove.
        listener: NonZeroUsize,

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
    pub(crate) fn apply(self, list: &mut List) -> Option<Task> {
        match self {
            Node::AddListener { task_waiting } => {
                // Add a new entry to the list.
                let entry = Entry::new();
                let key = list.insert(entry);

                // Send the new key to the listener and wake it if necessary.
                task_waiting.entry_id.store(key.get(), Ordering::Release);
                return task_waiting.task.take();
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
                let state = list.remove(listener);

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
