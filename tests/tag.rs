//! Tests relating to tagging.

use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::task::{Context, Poll};

use event_listener::{Event, EventListener, IntoNotification};
use waker_fn::waker_fn;

fn notified<T>(listener: &mut EventListener<T>) -> Option<T> {
    let waker = waker_fn(|| ());
    match Pin::new(listener).poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(tag) => Some(tag),
        Poll::Pending => None,
    }
}

#[test]
fn notify_tag() {
    let event = Event::with_tag();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(notified(&mut l1).is_none());
    assert!(notified(&mut l2).is_none());
    assert!(notified(&mut l3).is_none());

    assert_eq!(event.notify(2.tag(true)), 2);
    assert_eq!(event.notify(1.tag(false)), 0);
    assert_eq!(notified(&mut l1), Some(true));
    assert_eq!(notified(&mut l2), Some(true));
    assert!(notified(&mut l3).is_none());
}

#[test]
fn notify_additional_tag() {
    let event = Event::with_tag();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(1.additional().tag(1)), 1);
    assert_eq!(event.notify(1.tag(2)), 0);
    assert_eq!(event.notify(1.additional().tag(3)), 1);

    assert_eq!(notified(&mut l1), Some(1));
    assert_eq!(notified(&mut l2), Some(3));
    assert!(notified(&mut l3).is_none());
}

#[test]
fn notify_with_tag() {
    let event = Event::with_tag();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(notified(&mut l1).is_none());
    assert!(notified(&mut l2).is_none());
    assert!(notified(&mut l3).is_none());

    let mut i = 0usize;

    event.notify(2.tag_with(|| {
        i += 1;
        i
    }));
    assert_eq!(notified(&mut l1), Some(1));
    assert_eq!(notified(&mut l2), Some(2));
    assert!(notified(&mut l3).is_none());
}

#[test]
fn drop_notify_with_tag() {
    let event = Event::with_tag();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(1.tag_with(|| 5i32)), 1);
    drop(l1);
    assert_eq!(notified(&mut l2), Some(5));
    assert!(notified(&mut l3).is_none());
}

#[test]
fn panic_with_tag() {
    let event = Event::with_tag();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();
    let mut l4 = event.listen();

    let p = panic::catch_unwind(|| {
        let mut i = 0usize;

        event.notify(4.tag_with(|| {
            i += 1;
            if i > 2 {
                panic!();
            }
            i
        }));
    });
    assert!(p.is_err());

    assert_eq!(notified(&mut l1), Some(1));
    assert_eq!(notified(&mut l2), Some(2));
    assert!(notified(&mut l3).is_none());
    assert!(notified(&mut l4).is_none());
}
