//! Test the wait() family of methods.

use event_listener::{Event, IntoNotification, Listener};
use std::time::{Duration, Instant};

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
fn wait_timeout_expiry() {
    let event = Event::new();
    let listener = event.listen();

    let start = Instant::now();
    assert_eq!(listener.wait_timeout(Duration::from_millis(200)), None);
    assert!(Instant::now().duration_since(start) >= Duration::from_millis(200));
}
