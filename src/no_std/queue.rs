//! An atomic queue of operations to process.

use super::node::Node;
use crate::sync::atomic::{AtomicPtr, Ordering};

use alloc::boxed::Box;
use core::ptr;

/// An naive atomic queue of operations to process.
pub(super) struct Queue<T> {
    /// The head of the queue.
    head: AtomicPtr<Link<T>>,

    /// The tail of the queue.
    tail: AtomicPtr<Link<T>>,
}

struct Link<T> {
    /// The inner node.
    node: Node<T>,

    /// The next node in the queue.
    next: AtomicPtr<Link<T>>,
}

impl<T> Queue<T> {
    /// Create a new, empty queue.
    pub(super) fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            tail: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Push a new node onto the queue.
    pub(super) fn push(&self, node: Node<T>) {
        // Allocate a new link.
        let link = Box::into_raw(Box::new(Link {
            node,
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        // Push the link onto the queue.
        let mut tail = self.tail.load(Ordering::Acquire);
        loop {
            // If the tail is null, then the queue is empty.
            if tail.is_null() {
                // Try to set the head to the new link.
                if self
                    .head
                    .compare_exchange(ptr::null_mut(), link, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // We successfully set the head, so we can set the tail.
                    self.tail.store(link, Ordering::Release);
                    return;
                }

                // The head was set by another thread, so we need to try again.
                tail = self.tail.load(Ordering::Acquire);
            }

            unsafe {
                // Try to set the next pointer of the current tail.
                let next = (*tail).next.load(Ordering::Acquire);
                if next.is_null()
                    && (*tail)
                        .next
                        .compare_exchange(
                            ptr::null_mut(),
                            link,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    // We successfully set the next pointer, so we can set the tail.
                    self.tail.store(link, Ordering::Release);
                    return;
                }
            }

            // The next pointer was set by another thread, so we need to try again.
            tail = self.tail.load(Ordering::Acquire);
        }
    }

    /// Pop a node from the queue.
    pub(super) fn pop(&self) -> Option<Node<T>> {
        // Pop the head of the queue.
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // If the head is null, then the queue is empty.
            if head.is_null() {
                return None;
            }

            unsafe {
                // Try to set the head to the next pointer.
                let next = (*head).next.load(Ordering::Acquire);
                if self
                    .head
                    .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // We successfully set the head, so we can set the tail if the queue is now empty.
                    if next.is_null() {
                        self.tail.store(ptr::null_mut(), Ordering::Release);
                    }

                    // Return the popped node.
                    let boxed = Box::from_raw(head);
                    return Some(boxed.node);
                }
            }

            // The head was set by another thread, so we need to try again.
            head = self.head.load(Ordering::Acquire);
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // Pop all nodes from the queue.
        while self.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use crate::notify::{GenericNotify, Internal, NotificationPrivate};
    use crate::sys::node::NothingProducer;

    use super::*;

    fn node_from_num(num: usize) -> Node<()> {
        Node::Notify(GenericNotify::new(num, true, NothingProducer::default()))
    }

    fn node_to_num(node: Node<()>) -> usize {
        match node {
            Node::Notify(notify) => notify.count(Internal::new()),
            _ => panic!("unexpected node"),
        }
    }

    #[test]
    fn push_pop() {
        let queue = Queue::new();

        queue.push(node_from_num(1));
        queue.push(node_from_num(2));
        queue.push(node_from_num(3));

        assert_eq!(node_to_num(queue.pop().unwrap()), 1);
        assert_eq!(node_to_num(queue.pop().unwrap()), 2);
        assert_eq!(node_to_num(queue.pop().unwrap()), 3);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn push_pop_many() {
        const COUNT: usize = if cfg!(miri) { 10 } else { 1_000 };

        for i in 0..COUNT {
            let queue = Queue::new();

            for j in 0..i {
                queue.push(node_from_num(j));
            }

            for j in 0..i {
                assert_eq!(node_to_num(queue.pop().unwrap()), j);
            }

            assert!(queue.pop().is_none());
        }
    }

    #[cfg(not(miri))]
    #[test]
    fn push_pop_many_threads() {
        use crate::sync::Arc;

        const NUM_THREADS: usize = 3;
        const COUNT: usize = 50;

        let mut handles = Vec::new();
        let queue = Arc::new(Queue::new());

        for _ in 0..NUM_THREADS {
            let queue = queue.clone();

            handles.push(std::thread::spawn(move || {
                for i in 0..COUNT {
                    queue.push(node_from_num(i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut items = Vec::new();
        while let Some(node) = queue.pop() {
            items.push(node_to_num(node));
        }

        items.sort_unstable();
        for i in 0..COUNT {
            for j in 0..NUM_THREADS {
                assert_eq!(items[i * NUM_THREADS + j], i);
            }
        }
    }
}
