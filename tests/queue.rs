//! Tests involving the backup queue used under heavy contention.

use std::future::Future;
use std::pin::Pin;
use std::task::Context;

use event_listener::{Event, EventListener};
use waker_fn::waker_fn;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn insert_and_notify() {
    let event = Event::new();

    // Lock to simulate contention.
    let lock = event.__lock_event();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    event.notify(2);
    event.notify(1);

    // Unlock to simulate contention being released.
    drop(lock);

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn insert_then_contention() {
    let event = Event::new();

    // Allow the listeners to be created without contention.
    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    // Lock to simulate contention.
    let lock = event.__lock_event();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    event.notify(2);

    // Unlock to simulate contention being released.
    drop(lock);

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}
