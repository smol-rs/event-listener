use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;

use event_listener::{Event, EventListener, QueueStrategy};
use waker_fn::waker_fn;

#[cfg(target_family = "wasm")]
use wasm_bindgen_test::wasm_bindgen_test as test;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn notify_fifo() {
    notify(QueueStrategy::Fifo)
}

#[test]
fn notify_lifo() {
    notify(QueueStrategy::Lifo)
}

fn notify(queue_strategy: QueueStrategy) {
    let event = Event::new_with_queue_strategy(queue_strategy);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    assert_eq!(event.notify(2), 2);
    assert_eq!(event.notify(1), 0);

    match queue_strategy {
        QueueStrategy::Fifo => {
            assert!(is_notified(&mut l1));
            assert!(is_notified(&mut l2));
            assert!(!is_notified(&mut l3));
        }
        QueueStrategy::Lifo => {
            assert!(is_notified(&mut l3));
            assert!(is_notified(&mut l2));
            assert!(!is_notified(&mut l1));
        }
    }
}

#[test]
fn notify_additional_fifo() {
    notify_additional(QueueStrategy::Fifo)
}

#[test]
fn notify_additional_lifo() {
    notify_additional(QueueStrategy::Lifo)
}

fn notify_additional(queue_strategy: QueueStrategy) {
    let event = Event::new_with_queue_strategy(queue_strategy);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(1), 0);
    assert_eq!(event.notify_additional(1), 1);

    match queue_strategy {
        QueueStrategy::Fifo => {
            assert!(is_notified(&mut l1));
            assert!(is_notified(&mut l2));
            assert!(!is_notified(&mut l3));
        }
        QueueStrategy::Lifo => {
            assert!(is_notified(&mut l3));
            assert!(is_notified(&mut l2));
            assert!(!is_notified(&mut l1));
        }
    }
}

#[test]
fn notify_one_fifo() {
    notify_one(QueueStrategy::Fifo)
}

#[test]
fn notify_one_lifo() {
    notify_one(QueueStrategy::Lifo)
}

fn notify_one(queue_strategy: QueueStrategy) {
    let event = Event::new_with_queue_strategy(queue_strategy);

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(1), 1);
    match queue_strategy {
        QueueStrategy::Fifo => {
            assert!(is_notified(&mut l1));
            assert!(!is_notified(&mut l2));
        }
        QueueStrategy::Lifo => {
            assert!(is_notified(&mut l2));
            assert!(!is_notified(&mut l1));
        }
    }

    assert_eq!(event.notify(1), 1);
    match queue_strategy {
        QueueStrategy::Fifo => assert!(is_notified(&mut l2)),
        QueueStrategy::Lifo => assert!(is_notified(&mut l1)),
    }
}

#[test]
fn notify_all_fifo() {
    notify_all(QueueStrategy::Fifo)
}

#[test]
fn notify_all_lifo() {
    notify_all(QueueStrategy::Lifo)
}

fn notify_all(queue_strategy: QueueStrategy) {
    let event = Event::new_with_queue_strategy(queue_strategy);

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(usize::MAX), 2);
    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
}

#[test]
fn drop_notified_fifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Fifo);

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified_lifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Lifo);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l3);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l1));
}

#[test]
fn drop_notified2_fifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Fifo);

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(2), 2);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified2_lifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Lifo);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let l3 = event.listen();

    assert_eq!(event.notify(2), 2);
    drop(l3);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l1));
}

#[test]
fn drop_notified_additional_fifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Fifo);

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
fn drop_notified_additional_lifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Lifo);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();
    let l4 = event.listen();

    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(2), 1);
    drop(l4);
    assert!(is_notified(&mut l3));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l1));
}

#[test]
fn drop_non_notified_fifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Fifo);

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l3);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
}

#[test]
fn drop_non_notified_lifo() {
    let event = Event::new_with_queue_strategy(QueueStrategy::Lifo);

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l1);
    assert!(is_notified(&mut l3));
    assert!(!is_notified(&mut l2));
}

#[test]
fn notify_all_fair_fifo() {
    notify_all_fair(QueueStrategy::Fifo)
}

#[test]
fn notify_all_fair_lifo() {
    notify_all_fair(QueueStrategy::Lifo)
}

fn notify_all_fair(queue_strategy: QueueStrategy) {
    let event = Event::new_with_queue_strategy(queue_strategy);
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

    match queue_strategy {
        QueueStrategy::Fifo => assert_eq!(&*v.lock().unwrap(), &[1, 2, 3]),
        QueueStrategy::Lifo => assert_eq!(&*v.lock().unwrap(), &[3, 2, 1]),
    }

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
