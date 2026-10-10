use core::convert::Infallible;
use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::ExecutablePath;

/// One argument in the fixed exact-test command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactTestArgument<'name> {
    /// The libtest exact-match flag.
    Exact,
    /// The borrowed test name passed after the exact-match flag.
    Name(&'name str),
}

impl ExactTestArgument<'_> {
    /// Returns the literal argument consumed by the host launch adapter.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Exact => "--exact",
            Self::Name(name) => name,
        }
    }
}

/// Input to one exact-test intent construction step.
pub struct ExactTestIntentInput<'executable, 'scratch, 'name> {
    /// executable such as `target/debug/deps/parser-123`, borrowed so the caller retains artifact storage.
    pub executable: ExecutablePath<'executable, 'scratch>,
    /// test such as `parser::reads_header`, borrowed from its listing frame without copying bytes.
    pub test_name: &'name str,
}

/// Borrowed executable and ordered exact-test arguments for one child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactTestIntent<'executable, 'scratch, 'name> {
    executable: ExecutablePath<'executable, 'scratch>,
    test_name: &'name str,
}

impl<'executable, 'scratch, 'name> ExactTestIntent<'executable, 'scratch, 'name> {
    /// Returns the borrowed Cargo executable path.
    #[must_use]
    pub const fn executable(&self) -> ExecutablePath<'executable, 'scratch> {
        self.executable
    }

    /// Returns the borrowed selected test name.
    #[must_use]
    pub const fn test_name(&self) -> &'name str {
        self.test_name
    }

    /// Returns the fixed argv pair in libtest order without allocating.
    #[must_use]
    pub const fn arguments(&self) -> [ExactTestArgument<'name>; 2] {
        [
            ExactTestArgument::Exact,
            ExactTestArgument::Name(self.test_name),
        ]
    }
}

/// Pipe that forms one borrowed exact-test launch intent.
pub struct ExactTestIntentPipe<'executable, 'scratch, 'name>(
    PhantomData<(&'executable str, &'scratch str, &'name str)>,
);

impl<'executable, 'scratch, 'name> ExactTestIntentPipe<'executable, 'scratch, 'name> {
    /// Creates one exact-test intent step.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'executable, 'scratch, 'name> Pipe for ExactTestIntentPipe<'executable, 'scratch, 'name> {
    type In = ExactTestIntentInput<'executable, 'scratch, 'name>;
    type Out = ExactTestIntent<'executable, 'scratch, 'name>;
    type Err = Infallible;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            Ok(ExactTestIntent {
                executable: input.executable,
                test_name: input.test_name,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::string::String;

    use super::{ExactTestArgument, ExactTestIntentInput, ExactTestIntentPipe};
    use crate::ExecutablePath;
    use proxima_primitives::pipe::Pipe;

    fn form_intent<'executable, 'scratch, 'name>(
        executable: ExecutablePath<'executable, 'scratch>,
        test_name: &'name str,
    ) -> super::ExactTestIntent<'executable, 'scratch, 'name> {
        let pipe = ExactTestIntentPipe::new();
        let future = pipe.call(ExactTestIntentInput {
            executable,
            test_name,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(Ok(intent)) => intent,
            Poll::Ready(Err(error)) => match error {},
            Poll::Pending => panic!("exact-test intent pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn intent_borrows_path_and_name_and_rejects_transposed_arguments() {
        let executable = String::from("target/debug/deps/fixture");
        let test_name = String::from("alpha");
        let executable_path = executable.as_str();
        let selected_name = test_name.as_str();
        let (intent, intent_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| {
                form_intent(ExecutablePath::Borrowed(executable_path), selected_name)
            });
        let (arguments, argument_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| intent.arguments());
        let transposed = [
            ExactTestArgument::Name(selected_name),
            ExactTestArgument::Exact,
        ];

        assert_eq!(
            intent.executable(),
            ExecutablePath::Borrowed(executable_path)
        );
        assert_eq!(intent.test_name(), selected_name);
        assert_eq!(arguments[0].as_str(), "--exact");
        assert_eq!(arguments[1].as_str(), "alpha");
        match intent.executable() {
            ExecutablePath::Borrowed(path) => assert_eq!(path.as_ptr(), executable_path.as_ptr()),
            ExecutablePath::Scratch(_) => panic!("borrowed fixture path changed storage"),
        }
        assert_eq!(intent.test_name().as_ptr(), selected_name.as_ptr());
        assert_ne!(arguments, transposed);
        assert_eq!(intent_allocations, 0);
        assert_eq!(argument_allocations, 0);
        std::println!(
            "{{\"borrowed_executable_paths\":1,\"borrowed_test_names\":1,\"exact_arguments_in_order\":2,\"transposed_control_rejected\":1,\"allocations\":0}}"
        );
    }
}
