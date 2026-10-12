//! Windows edge for lowering the command line and environment block, plus the host path conventions.

pub const DYLIB_PATH_KEY: &str = "PATH";
pub const PATH_SEPARATOR: u8 = b';';
/// Whether the loader appends default library directories when the inherited search path is absent.
pub const LOADER_DEFAULT_FALLBACKS: bool = false;
pub const MAIN_SEPARATOR: u8 = b'\\';
pub const DEPS_SUFFIX: &str = "\\deps";

#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
mod lowering {
    //! Caller-owned UTF-16 command-line and environment storage for `CreateProcessW`.

    use core::cmp::Ordering;

    /// Bounded failures while lowering a borrowed intent for a Windows process launch.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsArgumentsError {
        /// An argument or environment entry contains an interior NUL.
        InteriorNul,
        /// An environment entry has no non-empty key followed by `=`.
        InvalidEnvironmentEntry,
        /// The caller's mutable command-line storage is too small.
        CommandLineStorageTooSmall { required: usize, available: usize },
        /// The caller's environment block storage is too small.
        EnvironmentStorageTooSmall { required: usize, available: usize },
        /// The caller's environment ordering storage is too small.
        EnvironmentOrderStorageTooSmall { required: usize, available: usize },
        /// Two child overrides name the same variable using Windows case folding.
        DuplicateEnvironmentOverride,
        /// The executable path is neither drive-absolute nor UNC, so `CreateProcessW` would search `PATH`.
        ExecutableNotAbsolute,
        /// An argument or environment entry is not valid Unicode, which UTF-16 launch data needs.
        NotUnicode,
    }

    /// Mutable, caller-owned buffers ready for `CreateProcessW`.
    #[derive(Debug, PartialEq, Eq)]
    pub struct WindowsArguments<'storage> {
        command_line: &'storage mut [u16],
        environment_block: &'storage mut [u16],
    }

    impl WindowsArguments<'_> {
        /// Returns the NUL-terminated mutable command line required by `CreateProcessW`.
        #[must_use]
        pub fn command_line(&mut self) -> &mut [u16] {
            self.command_line
        }

        /// Returns the sorted, double-NUL-terminated child environment block.
        #[must_use]
        pub fn environment_block(&self) -> &[u16] {
            self.environment_block
        }

        /// Borrows disjoint launch buffers together for one `CreateProcessW` call.
        pub fn create_process_buffers(&mut self) -> (&mut [u16], &[u16]) {
            (self.command_line, self.environment_block)
        }
    }

    /// Lowers one argument vector and environment overlay into caller-owned UTF-16 buffers.
    ///
    /// `arguments` is the whole command line, program name first. The environment ordering slice holds indexes into `inherited_environment`
    /// followed by `child_overrides`. All capacity errors are checked before either
    /// output buffer is changed.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsArgumentsError`] for interior NUL units, malformed or duplicate environment entries, or caller storage that is too small.
    pub fn lower_windows_arguments<'storage>(
        arguments: &[&str],
        inherited_environment: &[&str],
        child_overrides: &[&str],
        command_line: &'storage mut [u16],
        environment_block: &'storage mut [u16],
        environment_order: &mut [usize],
    ) -> Result<WindowsArguments<'storage>, WindowsArgumentsError> {
        let mut required_command_units = 1;
        for (index, argument) in arguments.iter().enumerate() {
            if argument.contains('\0') {
                return Err(WindowsArgumentsError::InteriorNul);
            }
            if index > 0 {
                required_command_units += 1;
            }
            required_command_units += quoted_argument_len(argument);
        }
        if command_line.len() < required_command_units {
            return Err(WindowsArgumentsError::CommandLineStorageTooSmall {
                required: required_command_units,
                available: command_line.len(),
            });
        }

        for entry in inherited_environment.iter().chain(child_overrides) {
            environment_key(entry)?;
        }
        for (override_index, override_entry) in child_overrides.iter().enumerate() {
            if child_overrides[..override_index]
                .iter()
                .any(|prior| same_environment_key(prior, override_entry))
            {
                return Err(WindowsArgumentsError::DuplicateEnvironmentOverride);
            }
        }
        let mut environment_count = 0;
        for inherited in inherited_environment {
            if !child_overrides
                .iter()
                .any(|override_entry| same_environment_key(inherited, override_entry))
            {
                environment_count += 1;
            }
        }
        environment_count += child_overrides.len();
        let mut required_environment_units = 1;
        for inherited in inherited_environment {
            if !child_overrides
                .iter()
                .any(|override_entry| same_environment_key(inherited, override_entry))
            {
                required_environment_units += inherited.encode_utf16().count() + 1;
            }
        }
        for override_entry in child_overrides {
            required_environment_units += override_entry.encode_utf16().count() + 1;
        }
        if environment_count == 0 {
            required_environment_units = 2;
        }
        if environment_order.len() < environment_count {
            return Err(WindowsArgumentsError::EnvironmentOrderStorageTooSmall {
                required: environment_count,
                available: environment_order.len(),
            });
        }
        let mut order_count = 0;
        for (index, inherited) in inherited_environment.iter().enumerate() {
            if !child_overrides
                .iter()
                .any(|override_entry| same_environment_key(inherited, override_entry))
            {
                environment_order[order_count] = index;
                order_count += 1;
            }
        }
        for index in 0..child_overrides.len() {
            environment_order[order_count] = inherited_environment.len() + index;
            order_count += 1;
        }
        if environment_block.len() < required_environment_units {
            return Err(WindowsArgumentsError::EnvironmentStorageTooSmall {
                required: required_environment_units,
                available: environment_block.len(),
            });
        }
        sort_environment(
            environment_order,
            order_count,
            inherited_environment,
            child_overrides,
        )?;

        let command_offset = write_command_line(arguments, command_line);
        write_environment_block(
            &environment_order[..order_count],
            inherited_environment,
            child_overrides,
            environment_block,
        );

        Ok(WindowsArguments {
            command_line: &mut command_line[..=command_offset],
            environment_block: &mut environment_block[..required_environment_units],
        })
    }

    /// Lowers byte launch data into growable UTF-16 buffers, sizing each from what the lowering reports it needs.
    ///
    /// `program` and `arguments` become the whole command line; the program is quoted like any argument.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsArgumentsError`] for interior NUL units, text that is not Unicode, or malformed or duplicate environment entries.
    #[cfg(feature = "edge-os")]
    pub fn lower_arguments<'storage>(
        program: &[u8],
        arguments: &[&[u8]],
        inherited: &[&[u8]],
        overrides: &[&[u8]],
        command_line: &'storage mut std::vec::Vec<u16>,
        environment_block: &'storage mut std::vec::Vec<u16>,
        environment_order: &mut std::vec::Vec<usize>,
    ) -> Result<WindowsArguments<'storage>, WindowsArgumentsError> {
        let argument_texts = texts(core::iter::once(program).chain(arguments.iter().copied()))?;
        let inherited_texts = texts(inherited.iter().copied())?;
        let override_texts = texts(overrides.iter().copied())?;
        loop {
            let lowered = lower_windows_arguments(
                &argument_texts,
                &inherited_texts,
                &override_texts,
                command_line,
                environment_block,
                environment_order,
            );
            match lowered.map(|_| ()) {
                Ok(()) => break,
                Err(WindowsArgumentsError::CommandLineStorageTooSmall { required, .. }) => {
                    command_line.resize(required, 0);
                }
                Err(WindowsArgumentsError::EnvironmentStorageTooSmall { required, .. }) => {
                    environment_block.resize(required, 0);
                }
                Err(WindowsArgumentsError::EnvironmentOrderStorageTooSmall { required, .. }) => {
                    environment_order.resize(required, 0);
                }
                Err(error) => return Err(error),
            }
        }
        lower_windows_arguments(
            &argument_texts,
            &inherited_texts,
            &override_texts,
            command_line,
            environment_block,
            environment_order,
        )
    }

    #[cfg(feature = "edge-os")]
    fn texts<'bytes>(
        entries: impl Iterator<Item = &'bytes [u8]>,
    ) -> Result<std::vec::Vec<&'bytes str>, WindowsArgumentsError> {
        entries
            .map(|entry| core::str::from_utf8(entry).map_err(|_| WindowsArgumentsError::NotUnicode))
            .collect()
    }

    /// Encodes one test binary's working directory as the NUL-terminated UTF-16 `CreateProcessW` expects.
    ///
    /// The directory is encoded once per binary and borrowed by every test launched from it.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsArgumentsError`] for an interior NUL or storage that is too small.
    pub fn write_working_directory<'storage>(
        directory: &str,
        storage: &'storage mut [u16],
    ) -> Result<&'storage [u16], WindowsArgumentsError> {
        if directory.contains('\0') {
            return Err(WindowsArgumentsError::InteriorNul);
        }
        let required = directory.encode_utf16().count() + 1;
        if storage.len() < required {
            return Err(WindowsArgumentsError::EnvironmentStorageTooSmall {
                required,
                available: storage.len(),
            });
        }
        for (cell, unit) in storage.iter_mut().zip(directory.encode_utf16()) {
            *cell = unit;
        }
        storage[required - 1] = 0;
        Ok(&storage[..required])
    }

    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the run job kills every child in it.
    pub const WINDOWS_JOB_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    /// `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`: no WerFault.exe process or crash dialog per child.
    pub const WINDOWS_JOB_DIE_ON_UNHANDLED_EXCEPTION: u32 = 0x0000_0400;
    /// `JOB_OBJECT_LIMIT_PRIORITY_CLASS`: the job's priority class applies to its processes.
    pub const WINDOWS_JOB_PRIORITY_CLASS: u32 = 0x0000_0020;
    /// `BELOW_NORMAL_PRIORITY_CLASS`: children yield cores to rustc.
    pub const WINDOWS_BELOW_NORMAL_PRIORITY: u32 = 0x0000_4000;

    const WINDOWS_EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const WINDOWS_CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const WINDOWS_CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;

    /// Limit flags of the one job every child of a run is created in.
    #[must_use]
    pub const fn windows_job_limit_flags() -> u32 {
        WINDOWS_JOB_KILL_ON_JOB_CLOSE
            | WINDOWS_JOB_DIE_ON_UNHANDLED_EXCEPTION
            | WINDOWS_JOB_PRIORITY_CLASS
    }

    /// `CreateProcessW` creation flags shared by every child launch.
    #[must_use]
    pub const fn windows_creation_flags() -> u32 {
        WINDOWS_EXTENDED_STARTUPINFO_PRESENT
            | WINDOWS_CREATE_NO_WINDOW
            | WINDOWS_CREATE_UNICODE_ENVIRONMENT
    }

    /// Attributes in the child's list: the stdio handle list, plus the job list when a job owns the run.
    #[must_use]
    pub const fn windows_attribute_count(has_job: bool) -> u32 {
        1 + has_job as u32
    }

    /// Whole `usize` words of caller storage that hold `required_bytes` of attribute list.
    #[must_use]
    pub const fn windows_attribute_storage_words(required_bytes: usize) -> usize {
        required_bytes.div_ceil(core::mem::size_of::<usize>())
    }

    /// Encodes one absolute executable path as the NUL-terminated UTF-16 `lpApplicationName` expects.
    ///
    /// Passing the path as the application name stops `CreateProcessW` from searching `PATH`.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsArgumentsError`] for an interior NUL, a path that is not drive-absolute or UNC, or storage that is too small.
    pub fn write_executable_path<'storage>(
        executable: &str,
        storage: &'storage mut [u16],
    ) -> Result<&'storage [u16], WindowsArgumentsError> {
        if !is_absolute_windows_path(executable) {
            return Err(WindowsArgumentsError::ExecutableNotAbsolute);
        }
        write_working_directory(executable, storage)
    }

    fn is_absolute_windows_path(path: &str) -> bool {
        let bytes = path.as_bytes();
        let drive_absolute = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/');
        drive_absolute || path.starts_with("\\\\")
    }

    fn write_command_line(arguments: &[&str], command_line: &mut [u16]) -> usize {
        let mut command_offset = 0;
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                command_line[command_offset] = u16::from(b' ');
                command_offset += 1;
            }
            command_offset += write_quoted_argument(argument, &mut command_line[command_offset..]);
        }
        command_line[command_offset] = 0;
        command_offset
    }

    fn write_environment_block(
        order: &[usize],
        inherited_environment: &[&str],
        child_overrides: &[&str],
        environment_block: &mut [u16],
    ) {
        let mut environment_offset = 0;
        for index in order {
            let entry = environment_entry(*index, inherited_environment, child_overrides);
            for unit in entry.encode_utf16() {
                environment_block[environment_offset] = unit;
                environment_offset += 1;
            }
            environment_block[environment_offset] = 0;
            environment_offset += 1;
        }
        environment_block[environment_offset] = 0;
        if order.is_empty() {
            environment_block[environment_offset + 1] = 0;
        }
    }

    fn quoted_argument_len(argument: &str) -> usize {
        let quote = argument.is_empty()
            || argument
                .encode_utf16()
                .any(|unit| matches!(unit, 0x20 | 0x09 | 0x22));
        let mut length = usize::from(quote) * 2;
        let mut backslashes = 0;
        for unit in argument.encode_utf16() {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                length += backslashes;
                if unit == u16::from(b'"') {
                    length += backslashes + 1;
                } else {
                    length += 1;
                }
                backslashes = 0;
            }
        }
        if quote {
            length += backslashes * 2;
        } else {
            length += backslashes;
        }
        length
    }

    fn write_quoted_argument(argument: &str, output: &mut [u16]) -> usize {
        let quote = argument.is_empty()
            || argument
                .encode_utf16()
                .any(|unit| matches!(unit, 0x20 | 0x09 | 0x22));
        let mut offset = 0;
        if quote {
            output[offset] = u16::from(b'"');
            offset += 1;
        }
        let mut backslashes = 0;
        for unit in argument.encode_utf16() {
            if unit == u16::from(b'\\') {
                backslashes += 1;
                continue;
            }
            if unit == u16::from(b'"') {
                for _ in 0..=backslashes * 2 {
                    output[offset] = u16::from(b'\\');
                    offset += 1;
                }
                output[offset] = unit;
                offset += 1;
            } else {
                for _ in 0..backslashes {
                    output[offset] = u16::from(b'\\');
                    offset += 1;
                }
                output[offset] = unit;
                offset += 1;
            }
            backslashes = 0;
        }
        let trailing_backslashes = if quote { backslashes * 2 } else { backslashes };
        for _ in 0..trailing_backslashes {
            output[offset] = u16::from(b'\\');
            offset += 1;
        }
        if quote {
            output[offset] = u16::from(b'"');
            offset += 1;
        }
        offset
    }

    fn environment_key(entry: &str) -> Result<&str, WindowsArgumentsError> {
        if entry.contains('\0') {
            return Err(WindowsArgumentsError::InteriorNul);
        }
        let Some(separator_offset) = entry
            .get(usize::from(entry.starts_with('='))..)
            .and_then(|tail| tail.find('='))
            .map(|offset| offset + usize::from(entry.starts_with('=')))
        else {
            return Err(WindowsArgumentsError::InvalidEnvironmentEntry);
        };
        if separator_offset == 0 || (entry.starts_with('=') && separator_offset == 1) {
            return Err(WindowsArgumentsError::InvalidEnvironmentEntry);
        }
        Ok(&entry[..separator_offset])
    }

    fn same_environment_key(first: &str, second: &str) -> bool {
        let Ok(first_key) = environment_key(first) else {
            return false;
        };
        let Ok(second_key) = environment_key(second) else {
            return false;
        };
        compare_environment_keys(first_key, second_key) == Ordering::Equal
    }

    fn compare_environment_keys(first: &str, second: &str) -> Ordering {
        let mut first_folded = first.chars().flat_map(char::to_lowercase);
        let mut second_folded = second.chars().flat_map(char::to_lowercase);
        loop {
            match (first_folded.next(), second_folded.next()) {
                (Some(first_character), Some(second_character)) => {
                    let ordering = first_character.cmp(&second_character);
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
                (None, None) => return Ordering::Equal,
            }
        }
    }

    fn sort_environment(
        order: &mut [usize],
        count: usize,
        inherited_environment: &[&str],
        child_overrides: &[&str],
    ) -> Result<(), WindowsArgumentsError> {
        for current in 1..count {
            let value = order[current];
            let mut position = current;
            while position > 0 {
                let value_key = environment_key(environment_entry(
                    value,
                    inherited_environment,
                    child_overrides,
                ))?;
                let previous_key = environment_key(environment_entry(
                    order[position - 1],
                    inherited_environment,
                    child_overrides,
                ))?;
                if compare_environment_keys(value_key, previous_key) != Ordering::Less {
                    break;
                }
                order[position] = order[position - 1];
                position -= 1;
            }
            order[position] = value;
        }
        Ok(())
    }

    fn environment_entry<'environment>(
        index: usize,
        inherited_environment: &[&'environment str],
        child_overrides: &[&'environment str],
    ) -> &'environment str {
        if index < inherited_environment.len() {
            inherited_environment[index]
        } else {
            child_overrides[index - inherited_environment.len()]
        }
    }

    #[cfg(test)]
    mod tests {

        use super::{WindowsArgumentsError, lower_windows_arguments, write_working_directory};
        use crate::cargo_message::TestArtifact;
        use crate::test_environment::{
            DYLIB_PATH_KEY, MAX_ENTRIES, TestEnvironmentInput, TestEnvironmentLayout,
        };

        fn exact_intent<'name>(executable: &'name str, test_name: &'name str) -> [&'name str; 3] {
            [executable, "--exact", test_name]
        }

        #[test]
        fn c43_quotes_executable_and_escapes_trailing_backslashes() {
            let intent = exact_intent("C:\\target dir\\fixture.exe", "alpha \\");
            let mut command_line = [0xA5; 128];
            let mut environment_block = [0xA5; 2];
            let mut environment_order = [];
            let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                lower_windows_arguments(
                    &intent,
                    &[],
                    &[],
                    &mut command_line,
                    &mut environment_block,
                    &mut environment_order,
                )
            });
            let mut lowered =
                result.expect("caller buffers fit command line and empty environment");
            let command_text = std::string::String::from_utf16(&*lowered.command_line())
                .expect("command line is utf-16");
            assert!(command_text.starts_with("\"C:\\target dir\\fixture.exe\" --exact \"alpha "));
            let command_units = lowered.command_line();
            assert_eq!(
                &command_units[command_units.len() - 4..],
                &[u16::from(b'\\'), u16::from(b'\\'), u16::from(b'"'), 0]
            );
            assert_eq!(lowered.environment_block(), &[0, 0]);
            assert_eq!(allocations, 0);
            std::println!(
                "{{\"quoted_path\":1,\"trailing_backslash_escape\":1,\"allocations\":{allocations}}}"
            );
        }

        #[test]
        fn c43_encodes_sorted_unicode_environment_override_in_caller_storage() {
            let intent = exact_intent("fixture.exe", "alpha");
            let inherited = ["PARENT=kept", "CARGO_VAL=old", "=C:=C:\\work"];
            let overrides = ["cargo_val=λ🚀"];
            let mut command_line = [0_u16; 64];
            let mut environment_block = [0_u16; 64];
            let mut environment_order = [usize::MAX; 3];
            let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                lower_windows_arguments(
                    &intent,
                    &inherited,
                    &overrides,
                    &mut command_line,
                    &mut environment_block,
                    &mut environment_order,
                )
            });
            let lowered = result.expect("caller buffers hold merged environment");
            let block = lowered.environment_block();
            assert_eq!(
                std::string::String::from_utf16(&block[..block.len() - 1])
                    .expect("environment is utf-16"),
                "=C:=C:\\work\0cargo_val=λ🚀\0PARENT=kept\0"
            );
            assert_eq!(block[block.len() - 1], 0);
            assert_eq!(allocations, 0);
            std::println!(
                "{{\"inherited_entries\":2,\"child_overrides\":1,\"unicode_value_units\":3,\"drive_directory_preserved\":1,\"allocations\":{allocations}}}"
            );
        }

        #[test]
        fn c43_rejects_short_utf16_storage_without_partial_writes() {
            let intent = exact_intent("C:\\target dir\\fixture.exe", "alpha");
            let mut command_line = [0xA5; 41];
            let original_command_line = command_line;
            let mut environment_block = [0xA5; 2];
            let original_environment_block = environment_block;
            let mut environment_order = [];
            let result = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            );
            assert!(matches!(
                result,
                Err(WindowsArgumentsError::CommandLineStorageTooSmall {
                    required: 42,
                    available: 41
                })
            ));
            assert_eq!(command_line, original_command_line);
            assert_eq!(environment_block, original_environment_block);
            std::println!("{{\"short_storage_rejected\":1,\"partial_writes\":0}}");
        }

        #[test]
        fn per_binary_environment_and_directory_lower_for_every_test_without_allocating() {
            let (executable, manifest_path, manifest_directory) = if cfg!(windows) {
                (
                    "C:\\work\\target\\debug\\deps\\dylib_env-3f2a1c9d8e7b6a50",
                    "C:\\work\\dylib_env\\Cargo.toml",
                    "C:\\work\\dylib_env",
                )
            } else {
                (
                    "/work/target/debug/deps/dylib_env-3f2a1c9d8e7b6a50",
                    "/work/dylib_env/Cargo.toml",
                    "/work/dylib_env",
                )
            };
            let artifact = TestArtifact {
                executable,
                package_id: "path+file:///work/dylib_env#0.1.0",
                manifest_path,
                target_name: "",
                target_kind: "",
            };
            let library_directories = ["/work/sysroot/lib"];
            let mut storage = [0_u8; 1024];
            let layout = TestEnvironmentLayout::build(
                &TestEnvironmentInput {
                    artifact,
                    linked_directories: "/work/target/debug/build/a/out",
                    library_directories: &library_directories,
                    inherited_dylib_path: Some("/usr/lib"),
                    home: None,
                    build_script: None,
                },
                &mut storage,
            )
            .expect("environment fits");
            let mut entries = [""; MAX_ENTRIES];
            for (index, entry) in entries.iter_mut().enumerate().take(layout.len()) {
                *entry = layout.entry(&storage, index).expect("index is below len");
            }
            let overrides = &entries[..layout.len()];
            let inherited = ["PARENT=kept", "HOME=/Users/dev"];
            let intent = exact_intent("fixture.exe", "alpha");
            let mut directory_units = [0_u16; 64];
            let mut command_line = [0_u16; 64];
            let mut environment_block = [0_u16; 1024];
            let mut environment_order = [usize::MAX; 16];

            let (checked, allocations) = crate::test_alloc::measure_allocations(|| {
                let directory = write_working_directory(
                    layout.current_directory(&storage),
                    &mut directory_units,
                )
                .map(<[u16]>::len);
                let mut lowered_blocks = 0;
                for _ in 0..100 {
                    let lowered = lower_windows_arguments(
                        &intent,
                        &inherited,
                        overrides,
                        &mut command_line,
                        &mut environment_block,
                        &mut environment_order,
                    );
                    lowered_blocks += usize::from(lowered.is_ok());
                }
                (directory, lowered_blocks)
            });

            assert_eq!(checked, (Ok(manifest_directory.len() + 1), 100));
            assert_eq!(allocations, 0);
            let lowered = lower_windows_arguments(
                &intent,
                &inherited,
                overrides,
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the environment");
            let block = std::string::String::from_utf16(lowered.environment_block())
                .expect("environment block is utf-16");
            assert!(
                block.contains(&std::format!("CARGO_MANIFEST_DIR={manifest_directory}\0")),
                "{block:?}"
            );
            assert!(block.contains(&std::format!(
                "{DYLIB_PATH_KEY}=/work/target/debug/build/a/out"
            )));
            assert!(block.contains("PARENT=kept\0"));
        }

        #[test]
        fn c67_job_limit_flags_kill_on_close_die_on_exception_and_priority() {
            let flags = super::windows_job_limit_flags();

            assert_eq!(flags, 0x2000 | 0x400 | 0x20);
            assert_eq!(flags & super::WINDOWS_JOB_KILL_ON_JOB_CLOSE, 0x2000);
            assert_eq!(flags & super::WINDOWS_JOB_DIE_ON_UNHANDLED_EXCEPTION, 0x400);
            assert_eq!(flags & super::WINDOWS_JOB_PRIORITY_CLASS, 0x20);
            assert_eq!(super::WINDOWS_BELOW_NORMAL_PRIORITY, 0x4000);
        }

        #[test]
        fn c67_creation_flags_hide_window_and_declare_unicode_environment() {
            let flags = super::windows_creation_flags();

            assert_eq!(flags, 0x0008_0000 | 0x0800_0000 | 0x0000_0400);
        }

        #[test]
        fn c67_attribute_count_grows_by_one_for_the_job_list() {
            assert_eq!(super::windows_attribute_count(false), 1);
            assert_eq!(super::windows_attribute_count(true), 2);
        }

        #[test]
        fn c67_attribute_storage_words_round_up_to_whole_words() {
            let word = core::mem::size_of::<usize>();

            assert_eq!(super::windows_attribute_storage_words(0), 0);
            assert_eq!(super::windows_attribute_storage_words(1), 1);
            assert_eq!(super::windows_attribute_storage_words(word), 1);
            assert_eq!(super::windows_attribute_storage_words(word + 1), 2);
            assert_eq!(
                super::windows_attribute_storage_words(72),
                72_usize.div_ceil(word)
            );
        }

        #[test]
        fn c67_executable_path_encodes_absolute_paths_nul_terminated() {
            let mut storage = [0xA5_u16; 128];
            let path = "C:\\Users\\runner\\work\\target\\debug\\deps\\cargo_val-0a1b2c3d4e5f.exe";

            let encoded = super::write_executable_path(path, &mut storage)
                .expect("drive-absolute path fits the caller storage");

            assert_eq!(encoded.last(), Some(&0));
            assert_eq!(encoded.len(), path.encode_utf16().count() + 1);
            assert!(
                encoded
                    .iter()
                    .copied()
                    .take(path.len())
                    .eq(path.encode_utf16())
            );
        }

        #[test]
        fn c67_executable_path_accepts_unc_and_verbatim_prefixes() {
            let mut storage = [0_u16; 64];

            assert!(super::write_executable_path("\\\\?\\C:\\a\\b.exe", &mut storage).is_ok());
            assert!(
                super::write_executable_path("\\\\build-host\\share\\b.exe", &mut storage).is_ok()
            );
        }

        #[test]
        fn c67_executable_path_rejects_relative_names_that_would_search_path() {
            let mut storage = [0_u16; 64];

            for relative in [
                "cargo.exe",
                ".\\deps\\a.exe",
                "\\rooted\\a.exe",
                "C:a.exe",
                "",
            ] {
                assert_eq!(
                    super::write_executable_path(relative, &mut storage),
                    Err(WindowsArgumentsError::ExecutableNotAbsolute),
                    "{relative:?}"
                );
            }
        }

        #[test]
        fn c67_executable_path_rejects_interior_nul_and_short_storage() {
            let mut storage = [0_u16; 4];

            assert_eq!(
                super::write_executable_path("C:\\a\0b.exe", &mut [0_u16; 32]),
                Err(WindowsArgumentsError::InteriorNul)
            );
            assert!(matches!(
                super::write_executable_path("C:\\a\\b.exe", &mut storage),
                Err(WindowsArgumentsError::EnvironmentStorageTooSmall { available: 4, .. })
            ));
        }
    }
}
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub use lowering::*;

#[cfg(all(windows, feature = "edge-os"))]
mod environment {
    //! The process environment, read once as UTF-8 `KEY=VALUE` entries for the runtime settings boundary.

    use std::string::String;
    use std::sync::OnceLock;
    use std::vec::Vec;

    use windows_sys::Win32::System::Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW};

    static SNAPSHOT: OnceLock<Vec<u8>> = OnceLock::new();

    /// A borrowed snapshot of the process environment, one NUL-terminated UTF-8 entry after another.
    pub struct Environment<'environment> {
        entries: &'environment [u8],
    }

    impl<'environment> Environment<'environment> {
        /// Reads the process environment block on first use and lends the snapshot afterwards.
        ///
        /// # Safety
        /// The caller must keep other threads from changing the process environment while the first read runs.
        #[must_use]
        pub unsafe fn current() -> Self {
            Self {
                entries: SNAPSHOT.get_or_init(read_block),
            }
        }

        fn entries(&self) -> impl Iterator<Item = &'environment [u8]> {
            self.entries
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
        }

        /// Lends each entry as `KEY=VALUE` bytes into fixed caller storage for child inheritance.
        ///
        /// # Errors
        ///
        /// Returns the entry count the environment holds, plus one, when `output` cannot hold every entry.
        pub fn collect(&self, output: &mut [&'environment [u8]]) -> Result<usize, usize> {
            let mut count = 0;
            for entry in self.entries() {
                if count == output.len() {
                    return Err(count + 1);
                }
                output[count] = entry;
                count += 1;
            }
            Ok(count)
        }

        /// Lends the bytes after `KEY=`, preserving empty-present versus absent; keys compare ASCII case-insensitively as Windows does.
        #[must_use]
        pub fn get(&self, key: &[u8]) -> Option<&'environment [u8]> {
            if key.is_empty() {
                return None;
            }
            self.entries().find_map(|entry| {
                let separator = entry.iter().skip(1).position(|byte| *byte == b'=')? + 1;
                let (name, rest) = entry.split_at(separator);
                name.eq_ignore_ascii_case(key).then(|| &rest[1..])
            })
        }
    }

    fn read_block() -> Vec<u8> {
        let block = unsafe { GetEnvironmentStringsW() };
        if block.is_null() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        let mut cursor = block.cast_const();
        loop {
            let mut length = 0;
            while unsafe { *cursor.add(length) } != 0 {
                length += 1;
            }
            if length == 0 {
                break;
            }
            let entry = unsafe { core::slice::from_raw_parts(cursor, length) };
            bytes.extend_from_slice(String::from_utf16_lossy(entry).as_bytes());
            bytes.push(0);
            cursor = unsafe { cursor.add(length + 1) };
        }
        unsafe { FreeEnvironmentStringsW(block) };
        bytes
    }
}
#[cfg(all(windows, feature = "edge-os"))]
pub use environment::*;
