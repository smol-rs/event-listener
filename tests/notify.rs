use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::usize;

use event_listener::{Event, EventListener};
use waker_fn::waker_fn;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn notify() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    event.notify(2);
    event.notify(1);

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

    event.notify_additional(1);
    event.notify(1);
    event.notify_additional(1);

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

    event.notify(1);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    event.notify(1);
    assert!(is_notified(&mut l2));
}

#[test]
fn notify_all() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    event.notify(usize::MAX);
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

    event.notify(2);
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

    event.notify_additional(1);
    event.notify(2);
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

    event.notify(1);
    drop(l3);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
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

    event.notify(usize::MAX);
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
