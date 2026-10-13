use crate::ExecutablePath;

/// Borrowed executable and selected test name for one exact-test child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactTestIntent<'executable, 'scratch, 'name> {
    /// executable such as `target/debug/deps/parser-123`, borrowed so the caller retains artifact storage.
    pub(crate) executable: ExecutablePath<'executable, 'scratch>,
    /// test such as `parser::reads_header`, borrowed from its listing frame without copying bytes.
    pub(crate) test_name: &'name str,
}

impl<'executable, 'scratch, 'name> ExactTestIntent<'executable, 'scratch, 'name> {
    /// Pairs a borrowed executable with the test selected from its listing.
    #[must_use]
    pub const fn new(
        executable: ExecutablePath<'executable, 'scratch>,
        test_name: &'name str,
    ) -> Self {
        Self {
            executable,
            test_name,
        }
    }

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
}

#[cfg(test)]
mod tests {
    use std::string::String;

    use super::ExactTestIntent;
    use crate::ExecutablePath;

    #[proxima::test]
    async fn intent_borrows_path_and_name_without_allocating() {
        let executable = String::from("target/debug/deps/fixture");
        let test_name = String::from("alpha");
        let executable_path = executable.as_str();
        let selected_name = test_name.as_str();
        let (intent, intent_allocations) = crate::test_alloc::measure_allocations(|| {
            ExactTestIntent::new(ExecutablePath::Borrowed(executable_path), selected_name)
        });

        assert_eq!(
            intent.executable(),
            ExecutablePath::Borrowed(executable_path)
        );
        assert_eq!(intent.test_name(), selected_name);
        match intent.executable() {
            ExecutablePath::Borrowed(path) => assert_eq!(path.as_ptr(), executable_path.as_ptr()),
            ExecutablePath::Scratch(_) => panic!("borrowed fixture path changed storage"),
        }
        assert_eq!(intent.test_name().as_ptr(), selected_name.as_ptr());
        assert_eq!(intent_allocations, 0);
        std::println!(
            "{{\"borrowed_executable_paths\":1,\"borrowed_test_names\":1,\"allocations\":0}}"
        );
    }
}
