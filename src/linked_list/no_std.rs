//! Implementation of the linked list using lock-free primitives.

extern crate std;

use crate::loom::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use crate::notify::{GenericNotify, Internal, Notification};

use core::cell::{Cell, UnsafeCell};
use core::cmp::Reverse;
use core::hint::spin_loop;
use core::iter;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::num::NonZeroUsize;
use core::ptr::{self, NonNull};
use core::slice;
use core::task::{Context, Poll, Waker};

use alloc::boxed::Box;
use alloc::collections::BinaryHeap;

/// The total number of buckets stored in each thread local.
/// All buckets combined can hold up to `usize::MAX - 1` entries.
const BUCKETS: usize = (usize::BITS - 1) as usize;

/// This listener has no data.
const NEW: usize = 0;

/// This listener is occupied.
const OCCUPIED: usize = 0b00001;

/// This listener is notified.
const NOTIFIED: usize = 0b00010;

/// This listener's notification is additional.
const ADDITIONAL: usize = 0b00100;

/// This listener is in the process of registering a new waker.
const REGISTERING: usize = 0b01000;

/// This listener is in the process of notifying a previous waker.
const NOTIFYING: usize = 0b10000;

/// State of a listener that was just removed.
enum NotificationState {
    /// There was no notification.
    Unnotified,

    /// We were notified with `notify()`
    Notified,

    /// We were notified with `notify_additional()`
    NotifiedAdditional,
}

/// Inner implementation of [`Event`].
pub(crate) struct Inner<T> {
    /// List of slots containing listeners.
    slots: Slots<T>,

    /// Free indexes for listeners.
    indexes: Indexes,

    /// Head of the linked list.
    head: AtomicUsize,

    /// End of the linked list.
    tail: AtomicUsize,

    /// Whether someone is notifying this list.
    ///
    /// Only one task can notify this event at a time. This task is called the "notifier".
    notifying: AtomicBool,

    /// Number of notifications.
    ///
    /// The notifier will check this for further notifications.
    standard_notify: AtomicUsize,

    /// Number of additional notifications.
    ///
    /// The notifier will check this for further notifications.
    additional_notify: AtomicUsize,
}

impl<T> Inner<T> {
    /// Create a new linked list.
    pub(crate) fn new() -> Self {
        Self {
            slots: Slots::new(),
            indexes: Indexes::new(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            notifying: AtomicBool::new(false),
            standard_notify: AtomicUsize::new(0),
            additional_notify: AtomicUsize::new(0),
        }
    }

    /// We never have enough info to tell this for sure.
    pub(crate) fn notifyable(&self, _limit: usize) -> bool {
        true
    }

    /// Insert a listener into this linked list.
    #[cold]
    pub(crate) fn listen(&self) -> Listener<T> {
        /// Update some local state and the current slot's tail at once.
        struct TailUpdater<'a> {
            tail: Cell<usize>,
            current_prev: &'a AtomicUsize,
        }

        impl TailUpdater<'_> {
            #[inline]
            fn tail(&self) -> usize {
                self.tail.get()
            }

            #[inline]
            fn update(&self, new_tail: usize, ordering: Ordering) {
                self.tail.set(new_tail);
                self.current_prev.store(new_tail, ordering);
            }
        }

        let (index, slot) = loop {
            let index = self.indexes.alloc();
            let slot = self.slots.get(index);

            // If a notification from last time is still being notified, allocate a new ID.
            if slot.state.load(Ordering::Acquire) & NOTIFYING != 0 {
                continue;
            } else {
                break (index, slot);
            }
        };

        let state = TailUpdater {
            tail: Cell::new(0),
            current_prev: &slot.prev,
        };

        // Set our link's state up.
        slot.state.store(OCCUPIED, Ordering::Relaxed);
        slot.next.store(0, Ordering::Release);
        state.update(self.tail.load(Ordering::Relaxed), Ordering::Relaxed);

        // Try appending this new node to the back of the list.
        loop {
            if state.tail() == 0 {
                // The list must be empty; try to make ourselves the head.
                let old_head = self
                    .head
                    .compare_exchange(0, index, Ordering::AcqRel, Ordering::Acquire)
                    .unwrap_or_else(|x| x);

                if old_head == 0 {
                    // Success! We are now the head. Set the tail and break.
                    self.tail.store(index, Ordering::Release);
                    break;
                } else {
                    // Use this head as our current tail and traverse the list.
                    state.update(old_head, Ordering::Acquire);
                }
            } else {
                // The tail's end should be 0. Set our index to it.
                let tail_slot = self.slots.get(state.tail());
                let old_tail = tail_slot
                    .next
                    .compare_exchange(0, index, Ordering::AcqRel, Ordering::Acquire)
                    .unwrap_or_else(|x| x);

                if old_tail == 0 {
                    // Success! We are now inserted. Set the tail to our slot number.
                    let _ = self.tail.compare_exchange(
                        state.tail(),
                        index,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    break;
                } else {
                    // The tail is occupied, move onto the next item and continue.
                    state.update(old_tail, Ordering::Acquire);
                }
            }

            // We may be trapped here for some time.
            spin_loop();
        }

        Listener {
            index: Some(NonZeroUsize::new(index).unwrap()),
            _phantom: PhantomData,
        }
    }

    pub(crate) fn notify(&self, notify: impl Notification<Tag = T>) -> usize {
        // T should always be `()`.
        debug_assert_eq!(mem::size_of::<T>(), 0);
        debug_assert!(!mem::needs_drop::<T>());

        // Get the count and the slot.
        let count = notify.count(Internal::new());
        let notification_slot = if notify.is_additional(Internal::new()) {
            &self.additional_notify
        } else {
            &self.standard_notify
        };

        // Bump the count.
        notification_slot.fetch_add(count, Ordering::SeqCst);

        // Try to become the notifier for a while.
        let mut no_lock = true;
        for _ in 0..128 {
            no_lock = self
                .notifying
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|x| x);
            if !no_lock {
                break;
            }

            core::hint::spin_loop();
        }

        if no_lock {
            // We did not capture the lock. The notifier should see the above addition and keep
            // going.
            return 0;
        }

        // We have the lock! Make sure to release it when we're done.
        let _guard = CallOnDrop(|| self.notifying.store(false, Ordering::Release));

        // Notify the actual wakers.
        self.notify_locked(count)
    }

    #[inline]
    pub(crate) fn remove(&self, listener: &mut Listener<T>) {
        let is_additional = match self.take(listener) {
            NotificationState::Unnotified => return,
            NotificationState::Notified => false,
            NotificationState::NotifiedAdditional => true,
        };

        // Propagate the notification.
        self.notify(GenericNotify::new(1, is_additional, || unreachable!()));
    }

    #[inline]
    pub(crate) fn discard(&self, listener: &mut Listener<T>) -> bool {
        !matches!(self.take(listener), NotificationState::Unnotified)
    }

    #[inline]
    pub(crate) fn poll(&self, listener: &mut Listener<T>, cx: &mut Context<'_>) -> Poll<T> {
        let index = match listener.index {
            Some(index) => index.get(),
            None => unreachable!("cannot poll a completed `EventListener` future"),
        };
        let slot = self.slots.get(index);

        // Tell whether or not we have been notified.
        if slot.state.load(Ordering::Acquire) & NOTIFIED != 0 {
            // We have succeeded!
            // SAFETY: T should *always* be ()
            debug_assert_eq!(mem::size_of::<T>(), 0);
            debug_assert!(!mem::needs_drop::<T>());
            self.take(listener);
            Poll::Ready(unsafe { mem::zeroed() })
        } else {
            // Register a waker and wait.
            slot.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    fn take(&self, listener: &mut Listener<T>) -> NotificationState {
        let index = match listener.index.take() {
            Some(x) => x.get(),
            None => return NotificationState::Unnotified,
        };
        let slot = self.slots.get(index);

        // Mark this state as unoccupied.
        let state = slot.state.fetch_and(!OCCUPIED, Ordering::AcqRel);
        debug_assert_ne!(state & OCCUPIED, 0);
        debug_assert_eq!(state & REGISTERING, 0);

        let prev = slot.prev.swap(0, Ordering::AcqRel);
        let next = slot.next.load(Ordering::Acquire);

        // Unlink from the previous entry.
        if prev == 0 {
            self.head.store(next, Ordering::Release);
        } else {
            self.slots.get(prev).next.store(next, Ordering::Release);
        }
        if next == 0 {
            self.tail.store(prev, Ordering::Release);
        } else {
            self.slots.get(next).prev.store(prev, Ordering::Release);
        }

        // Free the index and let another user take the slot.
        self.indexes.free(index);

        match (state & NOTIFIED != 0, state & ADDITIONAL != 0) {
            (false, _) => NotificationState::Unnotified,
            (true, false) => NotificationState::Notified,
            (true, true) => NotificationState::NotifiedAdditional,
        }
    }

    fn notify_locked(&self, ret_limit: usize) -> usize {
        /// Whether to notify as normal or as additional.
        #[derive(Debug, Clone, Copy, PartialEq)]
        #[repr(usize)]
        enum NotifyMethod {
            Standard = 0,
            Additional = 1,
        }

        impl NotifyMethod {
            fn invert(self) -> Self {
                match self {
                    Self::Standard => Self::Additional,
                    Self::Additional => Self::Standard,
                }
            }

            fn notification_slot<T>(self, inner: &Inner<T>) -> &AtomicUsize {
                match self {
                    Self::Standard => &inner.standard_notify,
                    Self::Additional => &inner.additional_notify,
                }
            }

            /// Use this method to notify the list.
            fn notify<T>(self, inner: &Inner<T>, mut count: usize) -> usize {
                let mut notified = 0;
                let mut cursor = inner.head.load(Ordering::Acquire);
                let mut prev_cursor = cursor;

                while cursor != 0 && count > 0 {
                    // Get the slot at our cursor.
                    let slot = inner.slots.get(cursor);

                    // If the entry is in the progress of being destroyed, try again.
                    let state = slot.state.load(Ordering::Acquire);
                    if state & OCCUPIED == 0x0 {
                        if prev_cursor == cursor {
                            // We've hit a hole, stop notifying.
                            break;
                        } else {
                            // Load the previous next.
                            core::hint::spin_loop();
                            cursor = slot.next.load(Ordering::Acquire);
                            continue;
                        }
                    }
                    prev_cursor = cursor;

                    // If the entry is already notified, skip it and decrement the count if needed.
                    if state & NOTIFIED != 0x0 {
                        cursor = slot.next.load(Ordering::Acquire);
                        if let NotifyMethod::Standard = self {
                            count -= 1;
                        }
                        continue;
                    }

                    // Notify the entry.
                    slot.notify(self == NotifyMethod::Additional);

                    // Update counts and move on.
                    count -= 1;
                    notified += 1;
                    cursor = slot.next.load(Ordering::Acquire);
                }

                notified
            }
        }

        /// Make sure we don't miss a notification.
        struct NotifyState {
            current: NotifyMethod,
            progress_made: [bool; 2],
        }

        impl NotifyState {
            fn new() -> Self {
                Self {
                    current: NotifyMethod::Standard,
                    progress_made: [true, true],
                }
            }

            #[inline]
            fn make_progress(&mut self) {
                self.progress_made[self.current as usize] = true;
            }

            #[inline]
            fn next_method(&mut self) -> Option<NotifyMethod> {
                // If we have made no progress, break out.
                if self.progress_made == [false, false] {
                    return None;
                }

                // Replace our current progress with "false" and switch to the inverted.
                self.progress_made[self.current as usize] = false;
                self.current = self.current.invert();

                Some(self.current)
            }
        }

        let mut state = NotifyState::new();
        let mut notified = 0;

        while let Some(method) = state.next_method() {
            // Move out the count.
            let count = method.notification_slot(self).swap(0, Ordering::Acquire);
            if count > 0 {
                // We've made progress in this state.
                state.make_progress();

                // Notify `count` entries.
                notified += method.notify(self, count);
            }
        }

        core::cmp::min(notified, ret_limit)
    }
}

/// Inner implementation of [`EventListener`].
pub(crate) struct Listener<T> {
    /// Index into the listener.
    index: Option<NonZeroUsize>,

    /// We don't physically hold a `T`.
    _phantom: PhantomData<Box<T>>,
}

/// Inner listener data.
struct Link<T> {
    /// Next entry in the list.
    next: AtomicUsize,

    /// Previous entry in the list.
    prev: AtomicUsize,

    /// State of the link.
    state: AtomicUsize,

    /// Slot for the waker.
    waker: Cell<Option<Waker>>,

    /// `T` should always be `()`.
    _phantom: PhantomData<Box<T>>,
}

impl<T> Link<T> {
    fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            prev: AtomicUsize::new(0),
            state: AtomicUsize::new(NEW),
            waker: Cell::new(None),
            _phantom: PhantomData,
        }
    }

    /// Notify this particular listener.
    fn notify(&self, additional: bool) {
        let mask = if additional {
            NOTIFYING | NOTIFIED | ADDITIONAL
        } else {
            NOTIFYING | NOTIFIED
        };
        let old_state = self.state.fetch_or(mask, Ordering::SeqCst);

        // Remove the NOTIFYING flag once we're done.
        let _guard = CallOnDrop(|| {
            self.state.fetch_and(!NOTIFYING, Ordering::Release);
        });

        // Three possibilities here:
        // - NOTIFIED is set. Someone else beat us to it.
        // - REGISTERING is not set. In which case we can freely take and wake.
        // - REGISTERING is set. In which case it will wake the waker.
        if old_state & NOTIFIED != 0 {
            // Do nothing.
        } else if old_state & REGISTERING == 0 {
            // SAFETY: No one else is fighting for this waker.
            if let Some(waker) = self.waker.take() {
                // In case waking the waker takes a while, make sure the slot is open.
                drop(_guard);
                waker.wake();
            }
        } else {
            // Do nothing. The task who set REGISTERING will wake the waker.
        }
    }

    /// Register a new waker into this listener.
    fn register(&self, waker: &Waker) {
        // Set the REGISTERING flag.
        let old_state = self.state.fetch_or(REGISTERING, Ordering::SeqCst);
        if old_state & NOTIFIED != 0 {
            // poll() somehow missed the notification. Wake the event loop and try again.
            waker.wake_by_ref();
            return;
        }

        // Unset REGISTERING before exiting out.
        let _guard = CallOnDrop(|| {
            let old_state = self.state.fetch_and(!REGISTERING, Ordering::SeqCst);
            if old_state & NOTIFIED != 0 {
                // There was a notification while we were registering. Wake our waker up.
                if let Some(waker) = self.waker.take() {
                    waker.wake();
                }
            }
        });

        // SAFETY: We have exclusive access to `self.waker`.
        match self.waker.take() {
            Some(w) if waker.will_wake(&w) => {
                // Put it back.
                self.waker.set(Some(w));
            }

            _ => self.waker.set(Some(waker.clone())),
        }
    }
}

/// Atomically expandable list of slots.
struct Slots<T> {
    /// Buckets in the list.
    buckets: [AtomicPtr<Link<T>>; BUCKETS],
}

impl<T> Slots<T> {
    fn new() -> Self {
        unsafe {
            // Create an empty array.
            let mut buckets: [MaybeUninit<AtomicPtr<Link<T>>>; BUCKETS] = {
                let raw: MaybeUninit<[AtomicPtr<Link<T>>; BUCKETS]> = MaybeUninit::uninit();
                // SAFETY: MaybeUninit<[T; N]> has the same layout as [MaybeUninit<T>; N]
                mem::transmute(raw)
            };

            // Initialize every bucket to null.
            for bucket in buckets.iter_mut() {
                *bucket = MaybeUninit::new(AtomicPtr::new(ptr::null_mut()))
            }

            // SAFETY: The array is now fully initialized.
            Self {
                buckets: mem::transmute(buckets),
            }
        }
    }

    /// Get the slot at the given index.
    fn get(&self, index: usize) -> &Link<T> {
        let bucket = self.bucket(index);
        let slot_index = index_to_slot_index(index);

        &bucket[slot_index]
    }

    /// Get the bucket for the index.
    #[inline]
    fn bucket(&self, index: usize) -> &[Link<T>] {
        let bucket_index = index_to_bucket(index);
        let size = bucket_index_to_size(bucket_index);

        // Load the pointer for the bucket.
        let bucket_ptr = unsafe {
            // SAFETY: `bucket` will never be less than `BUCKETS`
            self.buckets.get_unchecked(bucket_index)
        };
        let bucket = bucket_ptr.load(Ordering::Acquire);

        // If the bucket doesn't exist already, allocate it.
        let ptr = match NonNull::new(bucket) {
            Some(bucket) => bucket,

            None => {
                let new_bucket = iter::repeat_with(|| Link::<T>::new())
                    .take(size)
                    .collect::<Box<[_]>>();
                let new_bucket = Box::into_raw(new_bucket) as *mut Link<T>;

                // Try to replace it.
                let old_bucket = bucket_ptr
                    .compare_exchange(
                        ptr::null_mut(),
                        new_bucket,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .unwrap_or_else(|x| x);

                match NonNull::new(old_bucket) {
                    None => unsafe { NonNull::new_unchecked(new_bucket) },
                    Some(old_bucket) => {
                        // Drop the newly created bucket and use the old one.
                        drop(unsafe { Box::from_raw(slice::from_raw_parts_mut(new_bucket, size)) });
                        old_bucket
                    }
                }
            }
        };

        unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), size) }
    }
}

impl<T> Drop for Slots<T> {
    fn drop(&mut self) {
        // Free every bucket.
        for (i, bucket) in self.buckets.iter_mut().enumerate() {
            let bucket = *bucket.get_mut();
            if bucket.is_null() {
                continue;
            }

            // Drop the bucket.
            let size = bucket_index_to_size(i);
            drop(unsafe { Box::from_raw(slice::from_raw_parts_mut(bucket, size)) });
        }
    }
}

/// Available indexes into the list.
struct Indexes {
    /// List of indexes into our list.
    available: Lock<BinaryHeap<Reverse<usize>>>,

    /// The highest index we've produced so far.
    last: AtomicUsize,
}

impl Indexes {
    fn new() -> Self {
        Self {
            available: Lock::new(BinaryHeap::new()),
            last: AtomicUsize::new(1),
        }
    }

    /// Allocate a new index.
    fn alloc(&self) -> usize {
        self.available
            .access(|available| available.pop().map(|Reverse(x)| x))
            .flatten()
            .unwrap_or_else(|| self.last.fetch_add(1, Ordering::SeqCst))
    }

    /// Free an index.
    fn free(&self, index: usize) {
        self.available.access(|available| {
            available.push(Reverse(index));
        });
    }
}

/// An exclusive lock around some information.
struct Lock<T> {
    /// Whether we are locked.
    is_locked: AtomicBool,

    /// The information we are guarding.
    data: UnsafeCell<T>,
}

impl<T> Lock<T> {
    /// Create a new lock.
    fn new(data: T) -> Self {
        Self {
            is_locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Access the underlying data.
    fn access<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        // Lock the spinlock.
        if self
            .is_locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // Restore on drop.
            let _drop = CallOnDrop(|| self.is_locked.store(false, Ordering::Release));

            // SAFETY: We have exclusive access.
            Some(f(unsafe { &mut *self.data.get() }))
        } else {
            None
        }
    }
}

#[inline]
fn bucket_index_to_size(i: usize) -> usize {
    1 << i
}

#[inline]
fn index_to_bucket(i: usize) -> usize {
    usize::from(usize::BITS as u16) - ((i + 1).leading_zeros() as usize) - 1
}

#[inline]
fn index_to_slot_index(i: usize) -> usize {
    let size = bucket_index_to_size(index_to_bucket(i));
    i - (size - 1)
}

struct CallOnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type HashSet<K> = hashbrown::HashSet<K, ahash::RandomState>;

    #[test]
    fn lock() {
        let lock = Lock::new(());

        let x = lock.access(|()| {
            assert!(lock.access(|()| unreachable!()).is_none());
        });
        assert!(x.is_some());
    }

    #[test]
    fn slots() {
        let slots = Slots::<()>::new();
        let mut seen_ptrs: HashSet<usize> = HashSet::with_hasher(ahash::RandomState::default());

        // Don't exhaust our memory; only do this many.
        let count = 0xFFFF;
        for i in 1..count {
            let not_yet_seen = seen_ptrs.insert(slots.get(i) as *const Link<()> as usize);
            assert!(not_yet_seen);
        }
    }

    #[test]
    fn indexes_alloc() {
        let index = Indexes::new();
        let mut last = 0;

        for _ in 0..0xFFFF {
            let val = index.alloc();
            assert_eq!(val, last + 1);
            last = val;
        }
    }

    #[test]
    fn indexes_realloc() {
        let index = Indexes::new();

        assert_eq!(index.alloc(), 1);
        assert_eq!(index.alloc(), 2);
        assert_eq!(index.alloc(), 3);
        assert_eq!(index.alloc(), 4);

        index.free(3);
        index.free(2);

        assert_eq!(index.alloc(), 2);
        assert_eq!(index.alloc(), 3);
        assert_eq!(index.alloc(), 5);
        assert_eq!(index.alloc(), 6);
    }

    #[test]
    fn link_notify() {
        let link = Link::<()>::new();
        let waker = waker_fn::waker_fn(|| ());

        link.register(&waker);
        assert_eq!(link.state.load(Ordering::SeqCst), NEW);
        link.notify(false);
        assert_eq!(link.state.load(Ordering::SeqCst), NOTIFIED);
    }

    #[test]
    fn link_notify_additional() {
        let link = Link::<()>::new();
        let waker = waker_fn::waker_fn(|| ());

        link.register(&waker);
        assert_eq!(link.state.load(Ordering::SeqCst), NEW);
        link.notify(true);
        assert_eq!(link.state.load(Ordering::SeqCst), NOTIFIED | ADDITIONAL);
    }
}
