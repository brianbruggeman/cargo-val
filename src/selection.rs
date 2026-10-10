use core::convert::Infallible;
use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::dispatch_stream::DispatchStep;

/// Test-name matching strategy for a borrowed dispatch result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Match when the test name contains the filter text.
    Substring,
    /// Match only when the test name equals the filter text.
    Exact,
}

/// One filtered dispatch step, retaining the listing name borrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStep<'line> {
    /// All child slots are occupied; no listing input was consumed.
    Full,
    /// A matching test is ready to launch.
    Selected {
        /// Borrowed test name from the listing buffer.
        name: &'line str,
        /// Listing bytes consumed while finding this test.
        consumed_bytes: usize,
    },
    /// A test did not match the configured filter.
    Filtered {
        /// Borrowed name retained for accounting without copying.
        name: &'line str,
        /// Listing bytes consumed while finding this test.
        consumed_bytes: usize,
    },
    /// A non-test listing line was consumed.
    Ignored { consumed_bytes: usize },
    /// No complete listing line was available.
    NeedMore { consumed_bytes: usize },
}

/// Input to one borrowed-name selection step.
pub struct SelectionInput<'line> {
    /// Dispatch result produced from the listing stream.
    pub dispatch: DispatchStep<'line>,
    /// Filter text supplied by the caller.
    pub filter: &'line str,
    /// Matching strategy to apply to test names.
    pub mode: SelectionMode,
}

/// Pipe that selects borrowed test names without allocating.
pub struct SelectionPipe<'line>(PhantomData<&'line str>);

impl SelectionPipe<'_> {
    /// Creates one selection step.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl Default for SelectionPipe<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'line> Pipe for SelectionPipe<'line> {
    type In = SelectionInput<'line>;
    type Out = SelectionStep<'line>;
    type Err = Infallible;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            Ok(match input.dispatch {
                DispatchStep::Full => SelectionStep::Full,
                DispatchStep::Test {
                    name,
                    consumed_bytes,
                } => {
                    let selected = match input.mode {
                        SelectionMode::Substring => name.contains(input.filter),
                        SelectionMode::Exact => name == input.filter,
                    };
                    if selected {
                        SelectionStep::Selected {
                            name,
                            consumed_bytes,
                        }
                    } else {
                        SelectionStep::Filtered {
                            name,
                            consumed_bytes,
                        }
                    }
                }
                DispatchStep::Ignored { consumed_bytes } => {
                    SelectionStep::Ignored { consumed_bytes }
                }
                DispatchStep::NeedMore { consumed_bytes } => {
                    SelectionStep::NeedMore { consumed_bytes }
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::ptr;
    use core::task::{Context, Poll, Waker};
    use std::string::String;

    use super::{SelectionInput, SelectionMode, SelectionPipe, SelectionStep};
    use crate::dispatch_stream::DispatchStep;
    use proxima_primitives::pipe::Pipe;

    fn select<'line>(
        dispatch: DispatchStep<'line>,
        filter: &'line str,
        mode: SelectionMode,
    ) -> SelectionStep<'line> {
        let pipe = SelectionPipe::new();
        let future = pipe.call(SelectionInput {
            dispatch,
            filter,
            mode,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(Ok(step)) => step,
            Poll::Ready(Err(error)) => match error {},
            Poll::Pending => panic!("selection pipe unexpectedly yielded"),
        }
    }

    fn name_pointer(step: SelectionStep<'_>) -> *const u8 {
        match step {
            SelectionStep::Selected { name, .. } | SelectionStep::Filtered { name, .. } => {
                name.as_ptr()
            }
            SelectionStep::Full
            | SelectionStep::Ignored { .. }
            | SelectionStep::NeedMore { .. } => ptr::null(),
        }
    }

    #[test]
    fn selection_modes_preserve_borrowed_names_and_reject_exact_nonmatch() {
        let alpha = String::from("alpha");
        let alphabet = String::from("alphabet");
        let beta = String::from("beta");
        let alpha_name = alpha.as_str();
        let alphabet_name = alphabet.as_str();
        let beta_name = beta.as_str();

        let substring_alpha = select(
            DispatchStep::Test {
                name: alpha_name,
                consumed_bytes: 6,
            },
            "alpha",
            SelectionMode::Substring,
        );
        let substring_alphabet = select(
            DispatchStep::Test {
                name: alphabet_name,
                consumed_bytes: 9,
            },
            "alpha",
            SelectionMode::Substring,
        );
        let exact_alpha = select(
            DispatchStep::Test {
                name: alpha_name,
                consumed_bytes: 6,
            },
            "alpha",
            SelectionMode::Exact,
        );
        let exact_beta = select(
            DispatchStep::Test {
                name: beta_name,
                consumed_bytes: 5,
            },
            "alpha",
            SelectionMode::Exact,
        );

        assert_eq!(
            substring_alpha,
            SelectionStep::Selected {
                name: "alpha",
                consumed_bytes: 6,
            }
        );
        assert_eq!(
            substring_alphabet,
            SelectionStep::Selected {
                name: "alphabet",
                consumed_bytes: 9,
            }
        );
        assert_eq!(
            exact_alpha,
            SelectionStep::Selected {
                name: "alpha",
                consumed_bytes: 6,
            }
        );
        assert_eq!(
            exact_beta,
            SelectionStep::Filtered {
                name: "beta",
                consumed_bytes: 5,
            }
        );
        assert_eq!(name_pointer(substring_alpha), alpha_name.as_ptr());
        assert_eq!(name_pointer(substring_alphabet), alphabet_name.as_ptr());
        assert_eq!(name_pointer(exact_alpha), alpha_name.as_ptr());
        assert_eq!(name_pointer(exact_beta), beta_name.as_ptr());
        std::println!(
            "{{\"substring_selected\":2,\"exact_selected\":1,\"exact_nonmatch\":1,\"owned_name_copies\":0}}"
        );
    }
}
