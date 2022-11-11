//! The queue of nodes that keeps track of pending operations.

use crate::node::Node;
use crate::sync::atomic::{AtomicPtr, Ordering};

use crossbeam_utils::CachePadded;

use alloc::boxed::Box;
use core::ptr;

/// A queue of nodes.
pub(crate) struct Queue {
    /// The head of the queue.
    head: CachePadded<AtomicPtr<QueueNode>>,

    /// The tail of the queue.
    tail: CachePadded<AtomicPtr<QueueNode>>,
}

/// A single node in the `Queue`.
struct QueueNode {
    /// The next node in the queue.
    next: AtomicPtr<QueueNode>,

    /// Associated node data.
    node: Node,
}

impl Queue {
    /// Create a new queue.
    pub(crate) fn new() -> Self {
        Self {
            head: CachePadded::new(AtomicPtr::new(ptr::null_mut())),
            tail: CachePadded::new(AtomicPtr::new(ptr::null_mut())),
        }
    }

    /// Push a node to the tail end of the queue.
    pub(crate) fn push(&self, node: Node) {
        let node = Box::into_raw(Box::new(QueueNode {
            next: AtomicPtr::new(ptr::null_mut()),
            node,
        }));

        // Push the node to the tail end of the queue.
        let mut tail = self.tail.load(Ordering::Relaxed);

        // Get the next() pointer we have to overwrite.
        let next_ptr = if tail.is_null() {
            &self.head
        } else {
            unsafe { &(*tail).next }
        };

        loop {
            match next_ptr.compare_exchange(
                ptr::null_mut(),
                node,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Either set the tail to the new node, or let whoever beat us have it
                    let _ = self.tail.compare_exchange(
                        tail,
                        node,
                        Ordering::Release,
                        Ordering::Relaxed,
                    );

                    return;
                }
                Err(next) => tail = next,
            }
        }
    }

    /// Pop the oldest node from the head of the queue.
    pub(crate) fn pop(&self) -> Option<Node> {
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            if head.is_null() {
                return None;
            }

            let next = unsafe { (*head).next.load(Ordering::Relaxed) };

            match self
                .head
                .compare_exchange(head, next, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => {
                    // We have successfully popped the head of the queue.
                    let node = unsafe { Box::from_raw(head) };

                    // If next is also null, set the tail to null as well.
                    if next.is_null() {
                        let _ = self.tail.compare_exchange(
                            head,
                            ptr::null_mut(),
                            Ordering::Release,
                            Ordering::Relaxed,
                        );
                    }

                    return Some(node.node);
                }
                Err(h) => head = h,
            }
        }
    }
}
