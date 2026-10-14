use core::future::Future;

/// Drives `body` on a prime worker that wakes readers on hangup, as `main` does: the shared `#[proxima::test]` worker wakes on EPOLLIN only, so a pipe whose last writer left with no data pending never wakes its reader.
pub fn on_hangup_runtime<Body>(body: Body) -> Body::Output
where
    Body: Future + 'static,
    Body::Output: 'static,
{
    prime::os::core_shard::run_on_this_thread_as::<prime::os::reactor::Hangup, _>(proxima_runtime::CoreId(0), body).expect("the hangup prime root runs to completion")
}
