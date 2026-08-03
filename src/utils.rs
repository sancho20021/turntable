use crossbeam::channel::Sender;

/// Helper function to safely send events to a bounded queue without crashing or blocking.
///
/// # Arguments
/// * `queue` - The bounded channel sender
/// * `event` - The event item being pushed
/// * `context` - A descriptive label used in the log error message if the send fails
pub fn log_try_send<T>(queue: &Sender<T>, event: T, action: &str) {
    if let Err(e) = queue.try_send(event) {
        log::error!("failed to {}, try again: {}", action, e);
    }
}
