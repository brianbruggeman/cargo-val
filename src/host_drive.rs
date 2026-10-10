//! Prime's no_std local executor is the async coordinator task driver.

pub use prime::core::local_executor::{LocalExecutor as CoordinatorExecutor, TaskHandle};

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};
    use core::future::poll_fn;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Poll, Waker};
    use std::rc::Rc;
    use std::sync::Arc;

    use super::CoordinatorExecutor;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct WakeObservation {
        remote_wakes: usize,
        control_tokens: usize,
        resumed_tasks: usize,
    }

    fn matches_expected_wake(observation: WakeObservation) -> bool {
        observation
            == (WakeObservation {
                remote_wakes: 1,
                control_tokens: 1,
                resumed_tasks: 1,
            })
    }

    #[test]
    fn c36a_parks_pending_task_until_readiness_waker_then_resumes_it() {
        let executor = CoordinatorExecutor::new();
        let ready = Rc::new(Cell::new(false));
        let poll_count = Rc::new(Cell::new(0));
        let stored_waker = Rc::new(RefCell::new(None::<Waker>));
        let completed = Rc::new(Cell::new(false));
        let task_ready = Rc::clone(&ready);
        let task_poll_count = Rc::clone(&poll_count);
        let task_waker = Rc::clone(&stored_waker);
        let task_completed = Rc::clone(&completed);

        executor.spawn_local(async move {
            poll_fn(|context| {
                task_poll_count.set(task_poll_count.get() + 1);
                *task_waker.borrow_mut() = Some(context.waker().clone());
                if task_ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            task_completed.set(true);
        });

        executor.tick();
        assert_eq!(poll_count.get(), 1);
        assert!(!completed.get());
        assert_eq!(executor.tick(), 0);
        assert_eq!(poll_count.get(), 1);

        ready.set(true);
        stored_waker
            .borrow()
            .as_ref()
            .expect("pending task registered its waker")
            .wake_by_ref();
        executor.tick();

        assert_eq!(poll_count.get(), 2);
        assert!(completed.get());
        std::println!(
            "{{\"initial_pending\":1,\"repolls_before_event\":0,\"readiness_wakes\":1,\"ready_polls\":1}}"
        );
    }

    #[test]
    fn c36a_withheld_readiness_control_does_not_poll_pending_task_again() {
        let executor = CoordinatorExecutor::new();
        let poll_count = Rc::new(Cell::new(0));
        let task_poll_count = Rc::clone(&poll_count);

        executor.spawn_local(async move {
            poll_fn(|_| {
                task_poll_count.set(task_poll_count.get() + 1);
                Poll::<()>::Pending
            })
            .await;
        });

        executor.tick();
        let completion_polls = executor.tick();

        assert_eq!(completion_polls, 0);
        assert_eq!(poll_count.get(), 1);
        std::println!("{{\"withheld_event\":1,\"completion_polls\":0}}");
    }

    #[test]
    fn c36b_remote_wake_publishes_control_token_before_coordinator_park() {
        let remote_wakes = Arc::new(AtomicUsize::new(0));
        let callback_wakes = Arc::clone(&remote_wakes);
        let control_tokens = Arc::new(AtomicUsize::new(0));
        let callback_tokens = Arc::clone(&control_tokens);
        let executor = CoordinatorExecutor::with_remote_wake(Some(Arc::new(move || {
            callback_wakes.fetch_add(1, Ordering::SeqCst);
            callback_tokens.fetch_add(1, Ordering::SeqCst);
        })));
        let ready = Rc::new(Cell::new(false));
        let poll_count = Rc::new(Cell::new(0));
        let stored_waker = Rc::new(RefCell::new(None::<Waker>));
        let completed = Rc::new(Cell::new(false));
        let task_ready = Rc::clone(&ready);
        let task_poll_count = Rc::clone(&poll_count);
        let task_waker = Rc::clone(&stored_waker);
        let task_completed = Rc::clone(&completed);

        executor.spawn_local(async move {
            poll_fn(|context| {
                task_poll_count.set(task_poll_count.get() + 1);
                *task_waker.borrow_mut() = Some(context.waker().clone());
                if task_ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            task_completed.set(true);
        });
        executor.tick();
        assert_eq!(poll_count.get(), 1);

        ready.set(true);
        stored_waker
            .borrow()
            .as_ref()
            .expect("pending task registered its waker")
            .wake_by_ref();
        let tokens_before_park = control_tokens.load(Ordering::SeqCst);
        let tasks_polled_after_wake = executor.tick();
        let observation = WakeObservation {
            remote_wakes: remote_wakes.load(Ordering::SeqCst),
            control_tokens: tokens_before_park,
            resumed_tasks: usize::from(completed.get()),
        };

        assert_eq!(tokens_before_park, 1);
        assert_eq!(tasks_polled_after_wake, 1);
        assert_eq!(poll_count.get(), 2);
        assert!(completed.get());
        assert!(matches_expected_wake(observation));
        std::println!(
            "{{\"tasks_polled_after_wake\":{tasks_polled_after_wake},\"remote_wakes\":{},\"control_tokens_before_park\":{},\"resumed_tasks\":{},\"comparator_accepted\":1}}",
            observation.remote_wakes,
            observation.control_tokens,
            observation.resumed_tasks
        );
    }

    #[test]
    fn c36b_missing_control_token_is_a_lost_wake_control() {
        let remote_wakes = Arc::new(AtomicUsize::new(0));
        let control_tokens = Arc::new(AtomicUsize::new(0));
        let callback_wakes = Arc::clone(&remote_wakes);
        let executor = CoordinatorExecutor::with_remote_wake(Some(Arc::new(move || {
            callback_wakes.fetch_add(1, Ordering::SeqCst);
        })));
        let ready = Rc::new(Cell::new(false));
        let stored_waker = Rc::new(RefCell::new(None::<Waker>));
        let completed = Rc::new(Cell::new(false));
        let task_ready = Rc::clone(&ready);
        let task_waker = Rc::clone(&stored_waker);
        let task_completed = Rc::clone(&completed);

        executor.spawn_local(async move {
            poll_fn(|context| {
                *task_waker.borrow_mut() = Some(context.waker().clone());
                if task_ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            task_completed.set(true);
        });
        executor.tick();
        ready.set(true);
        stored_waker
            .borrow()
            .as_ref()
            .expect("pending task registered its waker")
            .wake_by_ref();
        let observed_remote_wakes = remote_wakes.load(Ordering::SeqCst);
        let delivered_control_tokens = control_tokens.load(Ordering::SeqCst);
        let observation = WakeObservation {
            remote_wakes: observed_remote_wakes,
            control_tokens: delivered_control_tokens,
            resumed_tasks: usize::from(completed.get()),
        };

        assert_eq!(observed_remote_wakes, 1);
        assert_eq!(delivered_control_tokens, 0);
        assert!(!completed.get());
        assert!(!matches_expected_wake(observation));
        std::println!(
            "{{\"remote_wakes\":{},\"control_tokens_before_park\":{},\"resumed_tasks\":{},\"comparator_rejected\":1}}",
            observation.remote_wakes,
            observation.control_tokens,
            observation.resumed_tasks
        );
    }
}
