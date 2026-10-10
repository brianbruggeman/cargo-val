//! Prime's no_std local executor is the async coordinator task driver.

pub use prime::core::local_executor::{LocalExecutor as CoordinatorExecutor, TaskHandle};

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};
    use core::future::poll_fn;
    use core::task::{Poll, Waker};
    use std::rc::Rc;

    use super::CoordinatorExecutor;

    #[test]
    fn parks_pending_task_until_readiness_waker_then_resumes_it() {
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
    fn withheld_readiness_control_does_not_poll_pending_task_again() {
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
}
