//! The `Notification` trait for specifying notification.

use crate::sync::atomic::{self, AtomicUsize, Ordering};
use core::fmt;

/// The type of notification to use with an [`Event`].
pub trait Notification {
    /// The tag data associated with a notification.
    type Tag;

    /// Emit a fence to ensure that the notification is visible to the listeners.
    fn fence(&self);

    /// Whether or not the number of currently waiting listeners should be subtracted from `count()`.
    fn is_additional(&self) -> bool;

    /// Get the number of listeners to wake.
    fn count(&self) -> usize;

    /// Get a tag to be associated with a notification.
    ///
    /// This method is expected to be called `count()` times.
    fn next_tag(&mut self) -> Self::Tag;
}

/// Notify a given number of unnotifed listeners.
#[derive(Debug, Clone)]
pub struct Notify(usize);

impl Notify {
    /// Create a new `Notify` with the given number of listeners to notify.
    fn new(count: usize) -> Self {
        Self(count)
    }
}

impl From<usize> for Notify {
    fn from(count: usize) -> Self {
        Self::new(count)
    }
}

impl Notification for Notify {
    type Tag = ();

    fn is_additional(&self) -> bool {
        false
    }

    fn fence(&self) {
        full_fence();
    }

    fn count(&self) -> usize {
        self.0
    }

    fn next_tag(&mut self) -> Self::Tag {}
}

/// Make the underlying notification additional.
#[derive(Debug, Clone)]
pub struct Additional<N: ?Sized>(N);

impl<N> Additional<N> {
    /// Create a new `Additional` with the given notification.
    fn new(inner: N) -> Self {
        Self(inner)
    }
}

impl<N> Notification for Additional<N>
where
    N: Notification + ?Sized,
{
    type Tag = N::Tag;

    fn is_additional(&self) -> bool {
        true
    }

    fn fence(&self) {
        self.0.fence();
    }

    fn count(&self) -> usize {
        self.0.count()
    }

    fn next_tag(&mut self) -> Self::Tag {
        self.0.next_tag()
    }
}

/// Don't emit a fence for this notification.
#[derive(Debug, Clone)]
pub struct Relaxed<N: ?Sized>(N);

impl<N> Relaxed<N> {
    /// Create a new `Relaxed` with the given notification.
    fn new(inner: N) -> Self {
        Self(inner)
    }
}

impl<N> Notification for Relaxed<N>
where
    N: Notification + ?Sized,
{
    type Tag = N::Tag;

    fn is_additional(&self) -> bool {
        self.0.is_additional()
    }

    fn fence(&self) {
        // Don't emit a fence.
    }

    fn count(&self) -> usize {
        self.0.count()
    }

    fn next_tag(&mut self) -> Self::Tag {
        self.0.next_tag()
    }
}

/// Use a tag to notify listeners.
#[derive(Debug, Clone)]
pub struct Tag<N: ?Sized, T> {
    tag: T,
    inner: N,
}

impl<N: ?Sized, T> Tag<N, T> {
    /// Create a new `Tag` with the given tag and notification.
    fn new(tag: T, inner: N) -> Self
    where
        N: Sized,
    {
        Self { tag, inner }
    }
}

impl<N, T> Notification for Tag<N, T>
where
    N: Notification + ?Sized,
    T: Clone,
{
    type Tag = T;

    fn is_additional(&self) -> bool {
        self.inner.is_additional()
    }

    fn fence(&self) {
        self.inner.fence();
    }

    fn count(&self) -> usize {
        self.inner.count()
    }

    fn next_tag(&mut self) -> Self::Tag {
        self.tag.clone()
    }
}

/// Use a function to generate a tag to notify listeners.
pub struct TagWith<N: ?Sized, F> {
    tag: F,
    inner: N,
}

impl<N: fmt::Debug, F> fmt::Debug for TagWith<N, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Ellipses;

        impl fmt::Debug for Ellipses {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("..")
            }
        }

        f.debug_struct("TagWith")
            .field("tag", &Ellipses)
            .field("inner", &self.inner)
            .finish()
    }
}

impl<N, F> TagWith<N, F> {
    /// Create a new `TagFn` with the given tag function and notification.
    fn new(tag: F, inner: N) -> Self {
        Self { tag, inner }
    }
}

impl<N, F, T> Notification for TagWith<N, F>
where
    N: Notification + ?Sized,
    F: FnMut() -> T,
{
    type Tag = T;

    fn is_additional(&self) -> bool {
        self.inner.is_additional()
    }

    fn fence(&self) {
        self.inner.fence();
    }

    fn count(&self) -> usize {
        self.inner.count()
    }

    fn next_tag(&mut self) -> Self::Tag {
        (self.tag)()
    }
}

/// A generic notification.
pub(crate) struct GenericNotify<F> {
    /// Number of listeners to notify.
    count: usize,

    /// Whether this notification is additional.
    additional: bool,

    /// Generate tags.
    tags: F,
}

impl<T, F: FnMut() -> T> GenericNotify<F> {
    pub(crate) fn new(count: usize, additional: bool, tags: F) -> Self {
        Self {
            count,
            additional,
            tags,
        }
    }
}

impl<T, F: FnMut() -> T> Notification for GenericNotify<F> {
    type Tag = T;

    fn is_additional(&self) -> bool {
        self.additional
    }

    fn fence(&self) {
        // Don't emit a fence.
    }

    fn count(&self) -> usize {
        self.count
    }

    fn next_tag(&mut self) -> Self::Tag {
        (self.tags)()
    }
}

/// A value that can be converted into a [`Notification`].
pub trait IntoNotification {
    /// The tag data associated with a notification.
    type Tag;

    /// The notification type.
    type Notify: Notification<Tag = Self::Tag>;

    /// Convert this value into a notification.
    fn into_notification(self) -> Self::Notify;

    /// Convert this value into an additional notification.
    fn additional(self) -> Additional<Self::Notify>
    where
        Self: Sized,
    {
        Additional::new(self.into_notification())
    }

    /// Don't emit a fence for this notification.
    fn relaxed(self) -> Relaxed<Self::Notify>
    where
        Self: Sized,
    {
        Relaxed::new(self.into_notification())
    }

    /// Use a tag with this notification.
    fn tag<T: Clone>(self, tag: T) -> Tag<Self::Notify, T>
    where
        Self: Sized + IntoNotification<Tag = ()>,
    {
        Tag::new(tag, self.into_notification())
    }

    /// Use a function to generate a tag with this notification.
    fn tag_with<T, F>(self, tag: F) -> TagWith<Self::Notify, F>
    where
        Self: Sized + IntoNotification<Tag = ()>,
        F: FnMut() -> T,
    {
        TagWith::new(tag, self.into_notification())
    }
}

impl<N: Notification> IntoNotification for N {
    type Tag = N::Tag;
    type Notify = N;

    fn into_notification(self) -> Self::Notify {
        self
    }
}

macro_rules! impl_for_numeric_types {
    ($($ty:ty)*) => {$(
        impl IntoNotification for $ty {
            type Tag = ();
            type Notify = Notify;

            #[allow(unused_comparisons)]
            fn into_notification(self) -> Self::Notify {
                if self < 0 {
                    panic!("negative notification count");
                }

                use core::convert::TryInto;
                Notify::new(self.try_into().expect("overflow"))
            }
        }
    )*};
}

impl_for_numeric_types! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 }

/// Equivalent to `atomic::fence(Ordering::SeqCst)`, but in some cases faster.
#[inline]
pub(super) fn full_fence() {
    if cfg!(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        not(miri)
    )) {
        // HACK(stjepang): On x86 architectures there are two different ways of executing
        // a `SeqCst` fence.
        //
        // 1. `atomic::fence(SeqCst)`, which compiles into a `mfence` instruction.
        // 2. `_.compare_exchange(_, _, SeqCst, SeqCst)`, which compiles into a `lock cmpxchg` instruction.
        //
        // Both instructions have the effect of a full barrier, but empirical benchmarks have shown
        // that the second one is sometimes a bit faster.
        //
        // The ideal solution here would be to use inline assembly, but we're instead creating a
        // temporary atomic variable and compare-and-exchanging its value. No sane compiler to
        // x86 platforms is going to optimize this away.
        atomic::compiler_fence(Ordering::SeqCst);
        let a = AtomicUsize::new(0);
        let _ = a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        atomic::compiler_fence(Ordering::SeqCst);
    } else {
        atomic::fence(Ordering::SeqCst);
    }
}
