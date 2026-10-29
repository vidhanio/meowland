//! Runtime-neutral cooperative scheduling for bounded I/O work.

use std::{future, task::Poll};

/// Yield once so other ready tasks and event sources can make progress.
pub async fn yield_now() {
    let mut yielded = false;
    future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}
