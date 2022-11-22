use core::pin::Pin;
use criterion::{criterion_group, criterion_main, Criterion};
use event_listener::{Event, EventListener};

const COUNT: usize = 8000;

fn bench_events(c: &mut Criterion) {
    c.bench_function("notify_and_wait", |b| {
        let ev = Event::new();
        let mut handles = core::iter::repeat_with(EventListener::new)
            .take(COUNT)
            .collect::<Vec<_>>();

        b.iter(|| {
            handles.iter_mut().for_each(|h| {
                // SAFETY: We never move the eventlistener out.
                let evl = unsafe { Pin::new_unchecked(h) };
                evl.listen_to(&ev);
            });

            ev.notify(COUNT);

            for handle in handles.iter_mut() {
                // SAFETY: We never move the eventlistener out.
                let evl = unsafe { Pin::new_unchecked(handle) };
                evl.wait();
            }
        });
    });
}

criterion_group!(benches, bench_events);
criterion_main!(benches);
