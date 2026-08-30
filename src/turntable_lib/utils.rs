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

pub fn unzip_array<const N: usize, T1, T2>(a: [(T1, T2); N]) -> ([T1; N], [T2; N]) {
    let (v1, v2): (Vec<T1>, Vec<T2>) = a.into_iter().unzip();

    (
        v1.try_into().unwrap_or_else(|_| unreachable!()),
        v2.try_into().unwrap_or_else(|_| unreachable!()),
    )
}

pub fn unzip_array3<const N: usize, T1, T2, T3>(
    a: [(T1, T2, T3); N],
) -> ([T1; N], [T2; N], [T3; N]) {
    let (v1, v2, v3) = a.into_iter().fold(
        (
            Vec::with_capacity(N),
            Vec::with_capacity(N),
            Vec::with_capacity(N),
        ),
        |(mut v1, mut v2, mut v3), (x, y, z)| {
            v1.push(x);
            v2.push(y);
            v3.push(z);
            (v1, v2, v3)
        },
    );

    (
        v1.try_into().unwrap_or_else(|_| unreachable!()),
        v2.try_into().unwrap_or_else(|_| unreachable!()),
        v3.try_into().unwrap_or_else(|_| unreachable!()),
    )
}
