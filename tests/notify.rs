use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;

use event_listener::{Event, EventListener, Listener};
use waker_fn::waker_fn;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn debug() {
    let event = Event::new();
    let fmt = format!("{:?}", &event);
    assert!(fmt.contains("Event"));

    let listener = event.listen();
    let fmt = format!("{:?}", &listener);
    assert!(fmt.contains("EventListener"));

    let fmt = format!("{:?}", &event);
    assert!(fmt.contains("Event"));
}

#[test]
fn notify() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!event.is_notified());
    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    assert_eq!(event.notify(2), 2);
    assert!(event.is_notified());
    assert_eq!(event.notify(1), 0);
    assert!(event.is_notified());
    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_additional() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!event.is_notified());
    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(1), 0);
    assert_eq!(event.notify_additional(1), 1);
    assert!(event.is_notified());

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_zero() {
    let event = Event::new();
    assert_eq!(event.notify(1), 0);
    assert!(!event.is_notified());
}

#[test]
fn notify_relaxed() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    assert_eq!(event.notify_relaxed(2), 2);
    assert_eq!(event.notify_relaxed(1), 0);
    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_additional_relaxed() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify_additional_relaxed(1), 1);
    assert_eq!(event.notify_relaxed(1), 0);
    assert_eq!(event.notify_additional_relaxed(1), 1);

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_one() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(1), 1);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(1), 1);
    assert!(is_notified(&mut l2));
}

#[test]
fn notify_all() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!event.is_notified());

    assert_eq!(event.notify(usize::MAX), 2);
    assert!(event.is_notified());
    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
}

#[test]
fn drop_notified() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    event.notify(1);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified2() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(2), 2);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified_additional() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();
    let mut l4 = event.listen();

    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(2), 1);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(is_notified(&mut l3));
    assert!(!is_notified(&mut l4));
}

#[test]
fn drop_non_notified() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l3);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
}

#[test]
fn discard() {
    let event = Event::default();

    let l1 = event.listen();
    assert!(!l1.discard());

    let l1 = event.listen();
    event.notify(1);
    assert!(l1.discard());

    let l1 = event.listen();
    event.notify_additional(1);
    assert!(l1.discard());

    let mut l1 = event.listen();
    event.notify(1);
    assert!(is_notified(&mut l1));
    assert!(!l1.discard());
}

#[test]
fn same_event() {
    let e1 = Event::new();
    let e2 = Event::new();

    let l1 = e1.listen();
    let l2 = e1.listen();
    let l3 = e2.listen();

    assert!(l1.listens_to(&e1));
    assert!(!l1.listens_to(&e2));
    assert!(l1.same_event(&l2));
    assert!(!l1.same_event(&l3));
}

#[test]
#[should_panic = "cannot poll a completed `EventListener` future"]
fn poll_twice() {
    let event = Event::new();
    let mut l1 = event.listen();
    event.notify(1);

    assert!(is_notified(&mut l1));

    // Panic here.
    is_notified(&mut l1);
}

#[test]
fn notify_all_fair() {
    let event = Event::new();
    let v = Arc::new(Mutex::new(vec![]));

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    let waker1 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(1))
    };
    let waker2 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(2))
    };
    let waker3 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(3))
    };

    assert!(Pin::new(&mut l1)
        .poll(&mut Context::from_waker(&waker1))
        .is_pending());
    assert!(Pin::new(&mut l2)
        .poll(&mut Context::from_waker(&waker2))
        .is_pending());
    assert!(Pin::new(&mut l3)
        .poll(&mut Context::from_waker(&waker3))
        .is_pending());

    assert_eq!(event.notify(usize::MAX), 3);
    assert_eq!(&*v.lock().unwrap(), &[1, 2, 3]);

    assert!(Pin::new(&mut l1)
        .poll(&mut Context::from_waker(&waker1))
        .is_ready());
    assert!(Pin::new(&mut l2)
        .poll(&mut Context::from_waker(&waker2))
        .is_ready());
    assert!(Pin::new(&mut l3)
        .poll(&mut Context::from_waker(&waker3))
        .is_ready());
}
