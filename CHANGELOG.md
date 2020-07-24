# Version 2.2.1

- Always keep the last waker in `EventListener::poll()`.

# Version 2.2.0

- Add `EventListener::same_event()`.

# Version 2.1.0

- Add `EventListener::listens_to()`.

# Version 2.0.1

- Replace `usize::MAX` with `std::usize::MAX`.

# Version 2.0.0

- Remove `Event::notify_one()` and `Event::notify_all()`.
- Add `Event::notify_relaxed()` and `Event::notify_additional_relaxed()`.
- Dropped notified `EventListener` now notifies one *or* one additional listener.

# Version 1.2.0

- Add `Event::notify_additional()`.

# Version 1.1.2

- Change a `Relaxed` load to `Acquire` load.

# Version 1.1.1

- Fix a bug in `EventListener::wait_timeout()`.

# Version 1.1.0

- Add `EventListener::notify()`.

# Version 1.0.1

- Reduce the complexity of `notify_all()` from O(n) to amortized O(1).
- Fix a bug where entries were notified in wrong order.
- Add tests.

# Version 1.0.0

- Initial version.
