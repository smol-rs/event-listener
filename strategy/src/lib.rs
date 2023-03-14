// SPDX-Licenser-Identifier: MIT OR Apache-2.0

//! A strategy for using the [`event-listener`] crate in both blocking
//! and non-blocking contexts.
//!
//! One of the stand-out features of the [`event-listener`] crate is
//! the ability to use it in both asynchronous and synchronous contexts.
//! However, sometimes using it like this causes a lot of boilerplate
//! to be duplicated. This crate aims to reduce that boilerplate by
//! providing an [`EventListenerFuture`] trait that implements
//! both blocking and non-blocking functionality.
//!
//! # Examples
//!
//! ```
//! use event_listener::{Event, EventListener};
//! use event_listener_strategy::{EventListenerFuture, FutureWrapper, Strategy};
//!
//! use std::pin::Pin;
//! use std::task::Poll;
//! use std::thread;
//! use std::sync::Arc;
//!
//! // A future that waits three seconds for an event to be fired.
//! fn wait_three_seconds() -> WaitThreeSeconds {
//!     let event = Event::new();
//!     let listener = event.listen();
//!
//!     thread::spawn(move || {
//!         thread::sleep(std::time::Duration::from_secs(3));
//!         event.notify(1);
//!     });
//!
//!     WaitThreeSeconds { listener: Some(listener) }
//! }
//!
//! struct WaitThreeSeconds {
//!     listener: Option<EventListener>,
//! }
//!
//! impl EventListenerFuture for WaitThreeSeconds {
//!     type Output = ();
//!
//!     fn poll_with_strategy<S: Strategy>(
//!         mut self: Pin<&mut Self>,
//!         strategy: &mut S,
//!         context: &mut S::Context,
//!     ) -> Poll<Self::Output> {
//!         match strategy.poll(self.listener.take().unwrap(), context) {
//!             Ok(()) => Poll::Ready(()),
//!             Err(listener) => {
//!                 self.listener = Some(listener);
//!                 Poll::Pending
//!            }
//!        }
//!     }
//! }
//!
//! // Use the future in a blocking context.
//! let future = wait_three_seconds();
//! future.wait();
//!
//! // Use the future in a non-blocking context.
//! futures_lite::future::block_on(async {
//!     let future = FutureWrapper::new(wait_three_seconds());
//!     future.await;
//! });
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use event_listener::EventListener;
use pin_utils::pin_mut;

/// A wrapper around an [`EventListenerFuture`] that can be easily exported for use.
/// 
/// This type implements [`Future`], has a `_new()` constructor, and a `wait()` method
/// that uses the [`Blocking`] strategy to poll the future until it is ready.
/// 
/// # Examples
/// 
/// ```
/// mod my_future {
///     use event_listener_strategy::{easy_wrapper, EventListenerFuture, Strategy}; 
///     use std::pin::Pin;
///     use std::task::Poll;
/// 
///     struct MyFuture;
/// 
///     impl EventListenerFuture for MyFuture {
///         type Output = ();
/// 
///         fn poll_with_strategy<S: Strategy>(
///             self: Pin<&mut Self>,
///             strategy: &mut S,
///             context: &mut S::Context,
///         ) -> Poll<Self::Output> {
///             /* ... */
/// #           Poll::Ready(())
///         }
///     }
/// 
///     easy_wrapper! {
///         /// A future that does something.
///         pub struct MyFutureWrapper(MyFuture => ());
///         /// Wait for it.
///         pub wait();
///     }
/// 
///     impl MyFutureWrapper {
///         /// Create a new instance of the future.
///         pub fn new() -> Self {
///             Self::_new(MyFuture) 
///         }
///     }
/// }
/// 
/// use my_future::MyFutureWrapper;
/// 
/// // Use the future in a blocking context.
/// let future = MyFutureWrapper::new();
/// future.wait();
/// 
/// // Use the future in a non-blocking context.
/// futures_lite::future::block_on(async {
///     let future = MyFutureWrapper::new();
///     future.await;
/// }); 
/// ```
#[macro_export]
macro_rules! easy_wrapper {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident ($inner:ty => $output:ty);
        $(#[$wait_meta:meta])*
        $wait_vis: vis wait();
    ) => {
        ::pin_project_lite::pin_project! {
            $(#[$meta])*
            $vis struct $name {
                #[pin]
                _inner: $crate::FutureWrapper<$inner>
            }
        }

        impl $name {
            fn _new(inner: $inner) -> Self {
                Self {
                    _inner: $crate::FutureWrapper::new(inner)
                }
            }

            $(#[$wait_meta])*
            $wait_vis fn wait(self) -> $output {
                use $crate::EventListenerFuture;
                self._inner.into_inner().wait()
            }
        }

        impl ::core::future::Future for $name {
            type Output = $output; 

            fn poll(
                self: ::core::pin::Pin<&mut Self>,
                context: &mut ::core::task::Context<'_>
            ) -> ::core::task::Poll<Self::Output> {
                self.project()._inner.poll(context)
            }
        }
    };
}

/// A future that runs using the [`event-listener`] crate.
///
/// This is similar to the [`Future`] trait from libstd, with one notable difference: it takes
/// a strategy that tells it whether to operate in a blocking or non-blocking context. The
/// `poll_with_strategy` method is the equivalent of the `poll` method in this regard; it uses
/// the [`Strategy`] trait to determine how to poll the future.
///
/// From here, there are two additional things one can do with this trait:
///
/// - The `wait` method, which uses the [`Blocking`] strategy to poll the future until it is
///   ready, blocking the current thread until it is.
/// - The [`FutureWrapper`] type, which implements [`Future`] and uses the [`NonBlocking`]
///   strategy to poll the future.
pub trait EventListenerFuture {
    /// The type of value produced on completion.
    type Output;

    /// Poll the future using the provided strategy.
    ///
    /// This function should use the `Strategy::poll` method to poll the future, and proceed
    /// based on the result.
    fn poll_with_strategy<S: Strategy>(
        self: Pin<&mut Self>,
        strategy: &mut S,
        context: &mut S::Context,
    ) -> Poll<Self::Output>;

    /// Wait for the future to complete, blocking the current thread.
    ///
    /// This function uses the [`Blocking`] strategy to poll the future until it is ready.
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    fn wait(self) -> Self::Output
    where
        Self: Sized,
    {
        let this = self;
        pin_mut!(this);

        loop {
            if let Poll::Ready(res) = this
                .as_mut()
                .poll_with_strategy(&mut Blocking::default(), &mut ())
            {
                return res;
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// A wrapper around an [`EventListenerFuture`] that implements [`Future`].
    ///
    /// [`Future`]: core::future::Future
    #[derive(Debug, Clone)]
    pub struct FutureWrapper<F: ?Sized> {
        #[pin]
        inner: F,
    }
}

impl<F: EventListenerFuture> FutureWrapper<F> {
    /// Create a new `FutureWrapper` from the provided future.
    pub fn new(inner: F) -> Self {
        Self { inner }
    }

    /// Consume the `FutureWrapper`, returning the inner future.
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: ?Sized> FutureWrapper<F> {
    /// Get a reference to the inner future.
    pub fn get_ref(&self) -> &F {
        &self.inner
    }

    /// Get a mutable reference to the inner future.
    pub fn get_mut(&mut self) -> &mut F {
        &mut self.inner
    }

    /// Get a pinned mutable reference to the inner future.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut F> {
        self.project().inner
    }

    /// Get a pinned reference to the inner future.
    pub fn get_pin_ref(self: Pin<&Self>) -> Pin<&F> {
        self.project_ref().inner
    }
}

impl<F: EventListenerFuture> From<F> for FutureWrapper<F> {
    fn from(inner: F) -> Self {
        Self { inner }
    }
}

impl<F: EventListenerFuture + ?Sized> Future for FutureWrapper<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.project()
            .inner
            .poll_with_strategy(&mut NonBlocking::default(), context)
    }
}

/// A strategy for polling an [`EventListenerFuture`] or an [`EventListener`].
///
/// This trait is used by the [`EventListenerFuture::poll_with_strategy`] method to determine
/// how to poll the future. It can also be used standalone, by calling the [`Strategy::wait`]
/// method.
///
/// [`EventListenerFuture::poll_with_strategy`]: EventListenerFuture::poll_with_strategy
/// [`EventListener`]: event_listener::EventListener
///
/// # Examples
///
/// ```
/// use event_listener::{Event, EventListener};
/// use event_listener_strategy::{EventListenerFuture, Strategy, Blocking, NonBlocking};
///
/// async fn wait_on<S: Strategy>(evl: EventListener, strategy: &mut S) {
///     strategy.wait(evl).await;
/// }
///
/// # futures_lite::future::block_on(async {
/// // Block on the future.
/// let ev = Event::new();
/// let listener = ev.listen();
/// ev.notify(1);
///
/// wait_on(listener, &mut Blocking::default()).await;
///
/// // Poll the future.
/// let listener = ev.listen();
/// ev.notify(1);
///
/// wait_on(listener, &mut NonBlocking::default()).await;
/// # });
/// ```
pub trait Strategy {
    /// The context needed to poll the future.
    type Context: ?Sized;

    /// The future returned by the [`Strategy::wait`] method.
    type Future: Future;

    /// Poll the event listener until it is ready.
    fn poll(
        &mut self,
        event_listener: EventListener,
        context: &mut Self::Context,
    ) -> Result<(), EventListener>;

    /// Wait for the event listener to become ready.
    fn wait(&mut self, evl: EventListener) -> Self::Future;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NonBlocking<'a> {
    _marker: PhantomData<Context<'a>>,
}

impl<'a> Strategy for NonBlocking<'a> {
    type Context = Context<'a>;
    type Future = EventListener;

    fn wait(&mut self, evl: EventListener) -> Self::Future {
        evl
    }

    fn poll(
        &mut self,
        mut event_listener: EventListener,
        context: &mut Self::Context,
    ) -> Result<(), EventListener> {
        match Pin::new(&mut event_listener).poll(context) {
            Poll::Ready(()) => Ok(()),
            Poll::Pending => Err(event_listener),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg(feature = "std")]
pub struct Blocking {
    _private: (),
}

impl Strategy for Blocking {
    type Context = ();
    type Future = core::future::Ready<()>;

    fn wait(&mut self, evl: EventListener) -> Self::Future {
        evl.wait();
        core::future::ready(())
    }

    fn poll(
        &mut self,
        event_listener: EventListener,
        _context: &mut Self::Context,
    ) -> Result<(), EventListener> {
        event_listener.wait();
        Ok(())
    }
}
