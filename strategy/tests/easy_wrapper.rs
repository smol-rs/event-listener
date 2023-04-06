//! Testing of the `easy_wrapper!` macro.

use std::{marker::PhantomData, pin::Pin, task::Poll};

use event_listener_strategy::{easy_wrapper, EventListenerFuture, Strategy};

#[test]
fn easy_wrapper_generics() {
    // Easy case.
    struct MyStrategy;

    impl EventListenerFuture for MyStrategy {
        type Output = ();

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&'a mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            Poll::Ready(())
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper(MyStrategy => ());
        wait();
    }

    MyEasyWrapper::_new(MyStrategy).wait();

    // Medium case with generics.
    struct MyStrategy2<T> {
        _marker: PhantomData<T>,
    }

    impl<T> EventListenerFuture for MyStrategy2<T> {
        type Output = T;

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&'a mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper2<T>(MyStrategy2<T> => T);
        wait();
    }

    // Medium mode with lifetime.
    struct MyStrategylt<'a> {
        _marker: PhantomData<&'a ()>,
    }

    impl<'a> EventListenerFuture for MyStrategylt<'a> {
        type Output = &'a ();

        fn poll_with_strategy<'b, S: Strategy<'b>>(
            self: Pin<&'b mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapperlt<'a>(MyStrategylt<'a> => &'a ());
        wait();
    }

    // Hard mode with generic bounds.
    struct MyStrategy3<'a, T: ?Sized>
    where
        T: 'a,
    {
        _marker: PhantomData<&'a T>,
    }

    impl<'a, T: ?Sized> EventListenerFuture for MyStrategy3<'a, T>
    where
        T: 'a,
    {
        type Output = &'a T;

        fn poll_with_strategy<'b, S: Strategy<'b>>(
            self: Pin<&'b mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper3<'a, T: ?Sized>(MyStrategy3<'a, T> => &'a T) where T: 'a;
        wait();
    }
}
