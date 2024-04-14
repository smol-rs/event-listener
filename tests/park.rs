//! Test the wait() family of methods.

#![cfg(feature = "std")]

use event_listener::{Event, EventListener, IntoNotification, Listener};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::thread;
use std::time::{Duration, Instant};

use waker_fn::waker_fn;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn total_listeners() {
    let event = Event::new();
    assert_eq!(event.total_listeners(), 0);

    let listener = event.listen();
    assert_eq!(event.total_listeners(), 1);

    drop(listener);
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn wait() {
    let event = Event::new();
    let listener = event.listen();

    assert_eq!(event.notify(1), 1);
    listener.wait();
}

#[test]
fn wait_tag() {
    let event = Event::with_tag();
    let listener = event.listen();

    assert_eq!(event.notify(1.tag(5i32)), 1);
    assert_eq!(listener.wait(), 5i32);
}

#[test]
fn wait_timeout() {
    let event = Event::new();
    let listener = event.listen();

    assert_eq!(event.notify(1), 1);
    assert_eq!(listener.wait_timeout(Duration::from_millis(50)), Some(()));
}

#[test]
fn wait_deadline() {
    let event = Event::new();
    let listener = event.listen();

    assert_eq!(event.notify(1), 1);
    assert_eq!(
        listener.wait_deadline(Instant::now() + Duration::from_millis(50)),
        Some(())
    );
}

#[test]
fn wait_timeout_expiry() {
    let event = Event::new();
    let listener = event.listen();

    let start = Instant::now();
    assert_eq!(listener.wait_timeout(Duration::from_millis(200)), None);
    assert!(Instant::now().duration_since(start) >= Duration::from_millis(200));
}

#[test]
fn unpark() {
    let event = Arc::new(Event::new());
    let listener = event.listen();

    thread::spawn({
        let event = event.clone();
        move || {
            thread::sleep(Duration::from_millis(100));
            event.notify(1);
        }
    });

    listener.wait();
}

#[test]
fn unpark_timeout() {
    let event = Arc::new(Event::new());
    let listener = event.listen();

    thread::spawn({
        let event = event.clone();
        move || {
            thread::sleep(Duration::from_millis(100));
            event.notify(1);
        }
    });

    let x = listener.wait_timeout(Duration::from_millis(200));
    assert!(x.is_some());
}

#[test]
#[should_panic = "cannot wait twice on an `EventListener`"]
fn wait_twice() {
    let event = Event::new();
    let mut listener = event.listen();
    event.notify(1);

    assert!(is_notified(&mut listener));

    // Panic here.
    listener.wait();
}
