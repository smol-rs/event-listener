//! The atomic queue containing listeners.

use crate::listener::Listener;
use crate::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crossbeam_utils::CachePadded;

use alloc::boxed::Box;

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};

/// A queue of listeners.
///
/// The operations on this queue that are defined are:
///
/// - Insertion at the tail of the list.
/// - Removal from the head of the list.
/// - Removing arbitrary elements from the list.
pub(crate) struct ListenerQueue {
    /// The head of the queue.
    head: CachePadded<AtomicPtr<Node>>,

    /// The tail of the queue.
    tail: CachePadded<AtomicPtr<Node>>,

    /// A single cached node.
    ///
    /// In theory, this helps the hot case where only a single listener is
    /// registered. In practice, I don't see much of a difference in benchmarks
    /// with or without this.
    cache: CachedNode,
}

// Ensure the bottom bits of the pointers are not used.
#[repr(align(4))]
pub(crate) struct Node {
    /// The listener in this node.
    listener: Listener,

    /// The next node in the queue.
    next: AtomicPtr<Node>,
}

/// A single cached node.
struct CachedNode {
    /// Whether or not the cache is occupied.
    occupied: AtomicBool,

    /// The cached node.
    node: UnsafeCell<MaybeUninit<Node>>,
}

impl ListenerQueue {
    /// Creates a new, empty queue.
    pub(crate) fn new() -> Self {
        Self {
            head: CachePadded::new(AtomicPtr::new(ptr::null_mut())),
            tail: CachePadded::new(AtomicPtr::new(ptr::null_mut())),
            cache: CachedNode {
                occupied: AtomicBool::new(false),
                node: UnsafeCell::new(MaybeUninit::uninit()),
            },
        }
    }

    /// Push a new node onto the queue.
    ///
    /// Returns a node pointer.
    pub(crate) fn push(&self, listener: Listener) -> NonNull<Node> {
        // Allocate a new node.
        let node = self.cache.alloc(listener);

        loop {
            // Get the pointer to the node.
            let tail = self.tail.load(Ordering::Acquire);

            // If the tail is empty, the list has to be empty.
            let next_ptr = if tail.is_null() {
                &self.head
            } else {
                unsafe { &(*tail).next }
            };

            // Try to set the next pointer.
            let next = next_ptr
                .compare_exchange(
                    ptr::null_mut(),
                    node.as_ptr(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .unwrap_or_else(|x| x);

            // See if the operation succeeded.
            if next.is_null() {
                // The node has been added, update the current tail.
                // If this fails, it means that another node already added themselves as the tail, which probably means
                // that they are the tail.
                self.tail
                    .compare_exchange(tail, node.as_ptr(), Ordering::AcqRel, Ordering::Acquire)
                    .ok();

                // Make sure the node knows it's enqueued.
                unsafe {
                    node.as_ref().listener.enqueue();
                }

                return node;
            } else {
                // The node has not been added, try again.
                continue;
            }
        }
    }

    /// Pop a node from the queue.
    pub(crate) fn pop(&self) -> Option<NonNull<Node>> {
        loop {
            // Get the head of the queue.
            let head = self.head.load(Ordering::Acquire);

            // If the head is null, the queue is empty.
            if head.is_null() {
                return None;
            }

            // Get the next pointer.
            let next = unsafe { (*head).next.load(Ordering::Acquire) };

            // Try to set the head to the next pointer.
            if self
                .head
                .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // If we just set the head to zero, set the tail to zero as well.
                if next.is_null() {
                    self.tail.store(ptr::null_mut(), Ordering::SeqCst);
                }

                // The head has been updated. Dequeue the old head.
                let head = unsafe { NonNull::new_unchecked(head) };
                if unsafe { head.as_ref().listener.dequeue() } {
                    // The EventListener for this node has been dropped, free the node.
                    unsafe { self.cache.dealloc(head) };
                } else {
                    // The event is still in use, return the node.
                    return Some(head);
                }
            }
        }
    }

    /// Orphan a node in the queue.
    ///
    /// The return value is Some if the node was notified, and true is the notification
    /// was an additional notification.
    pub(crate) unsafe fn orphan(&self, node: NonNull<Node>) -> Option<bool> {
        let (needs_drop, notified) = unsafe { node.as_ref().listener.orphan() };

        // If we need to deallocate the node, do so.
        if needs_drop {
            unsafe { self.cache.dealloc(node) };
        }

        notified
    }
}

impl CachedNode {
    /// Allocate a new node for a listener.
    fn alloc(&self, listener: Listener) -> NonNull<Node> {
        // Try to install a cached node.
        if !self.occupied.swap(true, Ordering::Acquire) {
            // We can now initialize the node.
            let node_ptr = self.node.get() as *mut Node;

            unsafe {
                // Initialize the node.
                node_ptr.write(Node::new(listener));

                // Return the node.
                NonNull::new_unchecked(node_ptr)
            }
        } else {
            // We failed to install a cached node, so we need to allocate a new
            // one.
            unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(Node::new(listener)))) }
        }
    }

    /// Deallocate a node.
    unsafe fn dealloc(&self, node: NonNull<Node>) {
        // Is this node the current cache node?
        if ptr::eq(self.node.get() as *const Node, node.as_ptr()) {
            // We can now clear the cache.
            unsafe { (self.node.get() as *mut Node).drop_in_place() };
            self.occupied.store(false, Ordering::Release);
        } else {
            // We need to deallocate the node on the heap.
            unsafe { Box::from_raw(node.as_ptr()) };
        }
    }
}

impl Node {
    fn new(listener: Listener) -> Self {
        Self {
            listener,
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(crate) fn listener(&self) -> &Listener {
        &self.listener
    }
}

impl Drop for ListenerQueue {
    fn drop(&mut self) {
        // Drain the queue.
        while self.pop().is_some() {}
    }
}
