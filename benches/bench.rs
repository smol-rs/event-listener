use criterion::{criterion_group, criterion_main, Criterion};
use event_listener::Event;

const COUNT: usize = 8000;

fn bench_events(c: &mut Criterion) {
    c.bench_function("notify_and_wait", |b| {
        let ev = Event::new();
        b.iter(|| {
            let mut handles = Vec::with_capacity(COUNT);

            for _ in 0..COUNT {
                handles.push(ev.listen());
            }

            ev.notify(COUNT);

            for handle in handles {
                handle.wait();
            }
        });
    });
}

criterion_group!(benches, bench_events);
criterion_main!(benches);
