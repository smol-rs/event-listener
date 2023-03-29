use std::pin::Pin;

use criterion::{criterion_group, criterion_main, Criterion};
use event_listener::{Event, EventListener};

const COUNT: usize = 8000;

fn bench_events(c: &mut Criterion) {
    c.bench_function("notify_and_wait", |b| {
        let ev = Event::new();
        let mut handles = Vec::with_capacity(COUNT);
        for _ in 0..COUNT {
            handles.push(EventListener::new(&ev));
        }

        b.iter(|| {
            for handle in &mut handles {
                // SAFETY: The handle is not moved out.
                let listener = unsafe { Pin::new_unchecked(handle) };
                listener.listen();
            }

            ev.notify(COUNT);

            for handle in &mut handles {
                // SAFETY: The handle is not moved out.
                let listener = unsafe { Pin::new_unchecked(handle) };
                listener.wait();
            }
        });
    });
}

criterion_group!(benches, bench_events);
criterion_main!(benches);
