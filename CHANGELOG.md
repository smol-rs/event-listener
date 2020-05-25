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
