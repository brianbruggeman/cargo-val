//! Unix edge for lowering argv, envp, and process environment, plus the host path conventions.

#[cfg(target_os = "macos")]
pub const DYLIB_PATH_KEY: &str = "DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
pub const DYLIB_PATH_KEY: &str = "LD_LIBRARY_PATH";

pub const PATH_SEPARATOR: u8 = b':';
/// Whether the loader appends default library directories when the inherited search path is absent.
pub const LOADER_DEFAULT_FALLBACKS: bool = cfg!(target_os = "macos");
pub const MAIN_SEPARATOR: u8 = b'/';
pub const DEPS_SUFFIX: &str = "/deps";

#[cfg(all(unix, feature = "edge-os"))]
mod lowering {
    //! Unix argv and envp lowered into C-string storage: fixed and caller-owned per test launch, allocated once per setup spawn.

    use core::ffi::{CStr, c_char};
    use core::ops::Range;
    use std::vec::Vec;

    use crate::build_defaults::{
        SPAWN_ARGUMENT_POINTERS, SPAWN_ENVIRONMENT_POINTERS, SPAWN_STRING_BYTES,
    };

    /// Why launch data cannot be represented as C strings in the storage offered.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ArgumentsError {
        /// The program, an argument, the directory, or an environment entry contains a NUL byte.
        InteriorNul,
        /// An environment entry has no non-empty key followed by `=`.
        InvalidEnvironmentEntry,
        /// The string storage cannot hold every string and its terminator.
        StringBytesTooSmall { required: usize, available: usize },
        /// The pointer storage cannot hold argv and its null sentinel.
        ArgumentPointersTooSmall { required: usize, available: usize },
        /// The pointer storage cannot hold the merged environment and its null sentinel.
        EnvironmentPointersTooSmall { required: usize, available: usize },
    }

    impl ArgumentsError {
        /// The refusal as terse text plus, for storage refusals, the required and available sizes.
        #[must_use]
        pub const fn describe(self) -> (&'static [u8], Option<(usize, usize)>) {
            match self {
                Self::InteriorNul => (b"an argument contains a NUL byte", None),
                Self::InvalidEnvironmentEntry => {
                    (b"an environment entry has no key before '='", None)
                }
                Self::StringBytesTooSmall {
                    required,
                    available,
                } => (b"argument bytes need", Some((required, available))),
                Self::ArgumentPointersTooSmall {
                    required,
                    available,
                } => (b"argument pointers need", Some((required, available))),
                Self::EnvironmentPointersTooSmall {
                    required,
                    available,
                } => (b"environment pointers need", Some((required, available))),
            }
        }
    }

    /// One child launch described by borrowed bytes.
    #[derive(Debug, Clone, Copy)]
    pub struct Launch<'launch> {
        /// Becomes `argv[0]` and the path handed to `posix_spawnp`.
        pub program: &'launch [u8],
        /// Arguments following the program name.
        pub arguments: &'launch [&'launch [u8]],
        /// Arguments following `arguments`, such as forwarded harness flags.
        pub trailing: &'launch [&'launch [u8]],
        /// Inherited environment entries in `KEY=VALUE` form.
        pub inherited: &'launch [&'launch [u8]],
        /// Child-only entries; each replaces any inherited entry with the same key.
        pub overrides: &'launch [&'launch [u8]],
        /// The child's working directory, when it differs from the parent's.
        pub directory: Option<&'launch [u8]>,
    }

    impl<'launch> Launch<'launch> {
        fn argument_strings(self) -> impl Iterator<Item = &'launch [u8]> + Clone {
            core::iter::once(self.program)
                .chain(self.arguments.iter().copied())
                .chain(self.trailing.iter().copied())
        }

        fn environment_strings(self) -> impl Iterator<Item = &'launch [u8]> + Clone {
            let overrides = self.overrides;
            self.inherited
                .iter()
                .copied()
                .filter(move |entry| !overridden(entry, overrides))
                .chain(overrides.iter().copied())
        }

        fn strings(self) -> impl Iterator<Item = &'launch [u8]> + Clone {
            self.argument_strings()
                .chain(self.environment_strings())
                .chain(self.directory)
        }
    }

    #[derive(Debug, Clone)]
    struct Layout {
        bytes: usize,
        argv: usize,
        envp: usize,
        program: Range<usize>,
        directory: Option<Range<usize>>,
    }

    impl Layout {
        fn measure(launch: Launch<'_>) -> Result<Self, ArgumentsError> {
            for entry in launch.inherited.iter().chain(launch.overrides) {
                environment_key(entry)?;
            }
            let mut bytes = 0;
            for text in launch.strings() {
                if text.contains(&0) {
                    return Err(ArgumentsError::InteriorNul);
                }
                bytes += text.len() + 1;
            }
            Ok(Self {
                bytes,
                argv: launch.argument_strings().count() + 1,
                envp: launch.environment_strings().count() + 1,
                program: 0..launch.program.len() + 1,
                directory: launch
                    .directory
                    .map(|text| bytes - text.len() - 1..bytes),
            })
        }

        fn check(
            &self,
            string_bytes: usize,
            argument_pointers: usize,
            environment_pointers: usize,
        ) -> Result<(), ArgumentsError> {
            if self.bytes > string_bytes {
                return Err(ArgumentsError::StringBytesTooSmall {
                    required: self.bytes,
                    available: string_bytes,
                });
            }
            if self.argv > argument_pointers {
                return Err(ArgumentsError::ArgumentPointersTooSmall {
                    required: self.argv,
                    available: argument_pointers,
                });
            }
            if self.envp > environment_pointers {
                return Err(ArgumentsError::EnvironmentPointersTooSmall {
                    required: self.envp,
                    available: environment_pointers,
                });
            }
            Ok(())
        }
    }

    /// NUL-terminated argv and envp pointer tables over one byte buffer, borrowed from the storage that holds them.
    #[derive(Debug)]
    pub struct Lowered<'storage> {
        bytes: &'storage [u8],
        argv: &'storage [*const c_char],
        envp: &'storage [*const c_char],
        layout: Layout,
    }

    impl<'storage> Lowered<'storage> {
        fn new(
            bytes: &'storage [u8],
            argv: &'storage [*const c_char],
            envp: &'storage [*const c_char],
            layout: Layout,
        ) -> Self {
            Self {
                bytes: &bytes[..layout.bytes],
                argv: &argv[..layout.argv],
                envp: &envp[..layout.envp],
                layout,
            }
        }

        /// Returns argv including its trailing null pointer.
        #[must_use]
        pub fn argv(&self) -> &[*const c_char] {
            self.argv
        }

        /// Returns the inherited environment overlaid with child-only entries, including its trailing null pointer.
        #[must_use]
        pub fn envp(&self) -> &[*const c_char] {
            self.envp
        }

        /// Returns every lowered string, each followed by its NUL.
        #[must_use]
        pub fn bytes(&self) -> &[u8] {
            self.bytes
        }

        /// Returns the program path as a C string.
        ///
        /// # Errors
        ///
        /// Returns [`ArgumentsError::InteriorNul`] when the stored program is not NUL-terminated exactly once.
        pub fn program(&self) -> Result<&CStr, ArgumentsError> {
            CStr::from_bytes_with_nul(&self.bytes[self.layout.program.clone()])
                .map_err(|_| ArgumentsError::InteriorNul)
        }

        /// Returns the working directory as a C string, when one was launched with.
        ///
        /// # Errors
        ///
        /// Returns [`ArgumentsError::InteriorNul`] when the stored directory is not NUL-terminated exactly once.
        pub fn directory(&self) -> Result<Option<&CStr>, ArgumentsError> {
            self.layout
                .directory
                .clone()
                .map(|range| {
                    CStr::from_bytes_with_nul(&self.bytes[range])
                        .map_err(|_| ArgumentsError::InteriorNul)
                })
                .transpose()
        }
    }

    /// Fixed argv, envp, and string storage sized by the build-time spawn constants, reused across launches.
    #[derive(Debug)]
    pub struct LaunchStorage {
        bytes: [u8; SPAWN_STRING_BYTES],
        argv: [*const c_char; SPAWN_ARGUMENT_POINTERS],
        envp: [*const c_char; SPAWN_ENVIRONMENT_POINTERS],
    }

    impl Default for LaunchStorage {
        fn default() -> Self {
            Self {
                bytes: [0; SPAWN_STRING_BYTES],
                argv: [core::ptr::null(); SPAWN_ARGUMENT_POINTERS],
                envp: [core::ptr::null(); SPAWN_ENVIRONMENT_POINTERS],
            }
        }
    }

    impl LaunchStorage {
        /// Lowers one launch into this storage without touching the heap.
        ///
        /// # Errors
        ///
        /// Returns [`ArgumentsError`] for an interior NUL byte, an environment entry without a key, or a launch that
        /// needs more string bytes or pointers than the storage holds, carrying the required and available sizes.
        pub fn lower(&mut self, launch: Launch<'_>) -> Result<Lowered<'_>, ArgumentsError> {
            let layout = Layout::measure(launch)?;
            layout.check(SPAWN_STRING_BYTES, SPAWN_ARGUMENT_POINTERS, SPAWN_ENVIRONMENT_POINTERS)?;
            write(launch, &mut self.bytes, &mut self.argv, &mut self.envp);
            Ok(Lowered::new(&self.bytes, &self.argv, &self.envp, layout))
        }
    }

    /// Argv, envp, and string storage allocated to fit one launch exactly, for spawns that run once per run.
    #[derive(Debug)]
    pub struct Arguments {
        bytes: Vec<u8>,
        argv: Vec<*const c_char>,
        envp: Vec<*const c_char>,
        layout: Layout,
    }

    impl Arguments {
        /// Allocates storage for one launch and lowers it.
        ///
        /// # Errors
        ///
        /// Returns [`ArgumentsError`] for an interior NUL byte or an environment entry without a key.
        pub fn lower(launch: Launch<'_>) -> Result<Self, ArgumentsError> {
            let layout = Layout::measure(launch)?;
            let mut bytes = std::vec![0; layout.bytes];
            let mut argv = std::vec![core::ptr::null(); layout.argv];
            let mut envp = std::vec![core::ptr::null(); layout.envp];
            write(launch, &mut bytes, &mut argv, &mut envp);
            Ok(Self {
                bytes,
                argv,
                envp,
                layout,
            })
        }

        /// Borrows the lowered launch.
        #[must_use]
        pub fn lowered(&self) -> Lowered<'_> {
            Lowered::new(&self.bytes, &self.argv, &self.envp, self.layout.clone())
        }
    }

    fn write(
        launch: Launch<'_>,
        bytes: &mut [u8],
        argv: &mut [*const c_char],
        envp: &mut [*const c_char],
    ) {
        let mut cursor = 0;
        for text in launch.strings() {
            bytes[cursor..cursor + text.len()].copy_from_slice(text);
            cursor += text.len();
            bytes[cursor] = 0;
            cursor += 1;
        }
        let bytes: &[u8] = bytes;
        let mut offset = 0;
        point(launch.argument_strings(), bytes, &mut offset, argv);
        point(launch.environment_strings(), bytes, &mut offset, envp);
    }

    fn point<'text>(
        texts: impl Iterator<Item = &'text [u8]>,
        bytes: &[u8],
        offset: &mut usize,
        slots: &mut [*const c_char],
    ) {
        let mut count = 0;
        for (slot, text) in slots.iter_mut().zip(texts) {
            *slot = bytes[*offset..].as_ptr().cast::<c_char>();
            *offset += text.len() + 1;
            count += 1;
        }
        if let Some(slot) = slots.get_mut(count) {
            *slot = core::ptr::null();
        }
    }

    fn overridden(entry: &[u8], overrides: &[&[u8]]) -> bool {
        let key = environment_key(entry).ok();
        overrides
            .iter()
            .any(|candidate| environment_key(candidate).ok() == key)
    }

    fn environment_key(entry: &[u8]) -> Result<&[u8], ArgumentsError> {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            return Err(ArgumentsError::InvalidEnvironmentEntry);
        };
        if separator == 0 {
            return Err(ArgumentsError::InvalidEnvironmentEntry);
        }
        Ok(&entry[..separator])
    }

    #[cfg(test)]
    mod tests {
        use core::ffi::{CStr, c_char};
        use std::string::String;
        use std::vec::Vec;

        use super::{Arguments, ArgumentsError, Launch, LaunchStorage};

        fn strings(pointers: &[*const c_char]) -> Vec<String> {
            pointers
                .iter()
                .take_while(|pointer| !pointer.is_null())
                .map(|pointer| {
                    unsafe { CStr::from_ptr(*pointer) }
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }

        fn launch<'a>(
            program: &'a [u8],
            arguments: &'a [&'a [u8]],
            inherited: &'a [&'a [u8]],
            overrides: &'a [&'a [u8]],
        ) -> Launch<'a> {
            Launch {
                program,
                arguments,
                trailing: &[],
                inherited,
                overrides,
                directory: None,
            }
        }

        #[test]
        fn c37_lowers_exact_argv_and_child_environment() {
            let lowered = Arguments::lower(launch(
                b"target/debug/deps/fixture",
                &[b"--exact", b"alpha"],
                &[b"PARENT=kept", b"CHILD=old"],
                &[b"CHILD=override"],
            ))
            .expect("launch data holds no NUL");
            let lowered = lowered.lowered();

            assert_eq!(
                strings(lowered.argv()),
                ["target/debug/deps/fixture", "--exact", "alpha"]
            );
            assert_eq!(strings(lowered.envp()), ["PARENT=kept", "CHILD=override"]);
            assert!(lowered.argv()[3].is_null());
            assert!(lowered.envp()[2].is_null());
            assert_eq!(&lowered.bytes()[..26], b"target/debug/deps/fixture\0");
            std::println!("{{\"inherited_entries\":1,\"child_overrides\":1,\"argv_pairs\":1}}");
        }

        #[test]
        fn c52_forwards_ignored_flag_after_the_exact_test_name() {
            let mut storage = LaunchStorage::default();
            let lowered = storage
                .lower(Launch {
                    trailing: &[b"--ignored"],
                    ..launch(
                        b"target/debug/deps/fixture",
                        &[b"--exact", b"fixture::ignored"],
                        &[],
                        &[],
                    )
                })
                .expect("launch data holds no NUL");

            assert_eq!(
                strings(lowered.argv()),
                [
                    "target/debug/deps/fixture",
                    "--exact",
                    "fixture::ignored",
                    "--ignored"
                ]
            );
            assert!(lowered.argv()[4].is_null());
            assert!(lowered.envp()[0].is_null());
            std::println!("{{\"exact_name_before_harness_flag\":1,\"ignored_flag_bytes\":9}}");
        }

        #[test]
        fn c37_lowers_the_working_directory_after_the_environment() {
            let mut storage = LaunchStorage::default();
            let lowered = storage
                .lower(Launch {
                    directory: Some(b"/work/package"),
                    ..launch(b"/bin/fixture", &[b"--exact", b"alpha"], &[b"A=1"], &[])
                })
                .expect("launch data holds no NUL");

            assert_eq!(lowered.program(), Ok(c"/bin/fixture"));
            assert_eq!(lowered.directory(), Ok(Some(c"/work/package")));
            assert_eq!(strings(lowered.envp()), ["A=1"]);
        }

        #[test]
        fn c37_rejects_an_interior_nul_in_the_test_name() {
            let result = Arguments::lower(launch(b"fixture", &[b"--exact", b"al\0pha"], &[], &[]));

            assert_eq!(result.unwrap_err(), ArgumentsError::InteriorNul);
            std::println!("{{\"interior_nul_control_rejected\":1}}");
        }

        #[test]
        fn c37_rejects_an_interior_nul_in_the_program() {
            let result = Arguments::lower(launch(b"fix\0ture", &[], &[], &[]));

            assert_eq!(result.unwrap_err(), ArgumentsError::InteriorNul);
        }

        #[test]
        fn c37_rejects_an_environment_entry_without_a_key() {
            let missing_separator = Arguments::lower(launch(b"fixture", &[], &[b"NOEQUALS"], &[]));
            let empty_key = Arguments::lower(launch(b"fixture", &[], &[], &[b"=value"]));

            assert_eq!(
                missing_separator.unwrap_err(),
                ArgumentsError::InvalidEnvironmentEntry
            );
            assert_eq!(
                empty_key.unwrap_err(),
                ArgumentsError::InvalidEnvironmentEntry
            );
        }

        #[test]
        fn c37_keeps_inherited_order_and_appends_overrides_last() {
            let lowered = Arguments::lower(launch(
                b"fixture",
                &[],
                &[b"A=1", b"B=2", b"C=3"],
                &[b"B=9", b"D=4"],
            ))
            .expect("launch data holds no NUL");

            assert_eq!(strings(lowered.lowered().envp()), ["A=1", "C=3", "B=9", "D=4"]);
        }

        #[test]
        fn c37_reports_required_and_available_when_arguments_exceed_the_pointer_storage() {
            let mut storage = LaunchStorage::default();
            let available = super::SPAWN_ARGUMENT_POINTERS;
            let flags = std::vec![b"--flag".as_slice(); available];

            let result = storage.lower(Launch {
                trailing: &flags,
                ..launch(b"fixture", &[b"--exact", b"alpha"], &[], &[])
            });

            assert_eq!(
                result.unwrap_err(),
                ArgumentsError::ArgumentPointersTooSmall {
                    required: available + 4,
                    available,
                }
            );
        }

        #[test]
        fn c37_reports_required_and_available_when_strings_exceed_the_byte_storage() {
            let mut storage = LaunchStorage::default();
            let available = super::SPAWN_STRING_BYTES;
            let name = std::vec![b'a'; available];

            let result = storage.lower(launch(b"fixture", &[b"--exact", &name], &[], &[]));

            assert_eq!(
                result.unwrap_err(),
                ArgumentsError::StringBytesTooSmall {
                    required: available + b"fixture\0--exact\0".len() + 1,
                    available,
                }
            );
        }

        #[test]
        fn c37_reports_required_and_available_when_environment_exceeds_the_pointer_storage() {
            let mut storage = LaunchStorage::default();
            let available = super::SPAWN_ENVIRONMENT_POINTERS;
            let entries = std::vec![b"K=v".as_slice(); available];
            let overrides = std::vec![b"A=1".as_slice(); 1];

            let result = storage.lower(launch(b"fixture", &[], &entries, &overrides));

            assert_eq!(
                result.unwrap_err(),
                ArgumentsError::EnvironmentPointersTooSmall {
                    required: available + 2,
                    available,
                }
            );
        }
    }
}
#[cfg(all(unix, feature = "edge-os"))]
pub use lowering::*;

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
mod environment {
    //! Borrowed Unix process environment values for the runtime settings boundary.

    use core::ffi::{CStr, c_char};
    use core::marker::PhantomData;

    /// A borrowed, null-terminated Unix environment pointer table.
    pub struct Environment<'environment> {
        entries: *const *const c_char,
        lifetime: PhantomData<&'environment CStr>,
    }

    impl<'environment> Environment<'environment> {
        /// Binds a process environment whose pointer table and strings remain stable.
        ///
        /// # Safety
        /// `entries` must be a null-terminated table of valid C strings, or null.
        /// Neither the table nor its strings may be changed for `'environment`.
        #[must_use]
        pub const unsafe fn from_raw(entries: *const *const c_char) -> Self {
            Self {
                entries,
                lifetime: PhantomData,
            }
        }

        /// Borrows the current macOS process environment without copying values.
        ///
        /// # Safety
        /// The caller must prevent process environment mutation while values borrowed
        /// through this view remain live.
        #[cfg(target_os = "macos")]
        #[must_use]
        pub unsafe fn current() -> Self {
            let pointer = unsafe { libc::_NSGetEnviron() };
            let entries = if pointer.is_null() {
                core::ptr::null()
            } else {
                unsafe { *pointer }.cast_const().cast()
            };
            unsafe { Self::from_raw(entries) }
        }

        /// Borrows the Linux process environment pointer table.
        ///
        /// # Safety
        /// The caller must keep the process environment unchanged while this view lives.
        #[must_use]
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        pub unsafe fn current() -> Self {
            unsafe extern "C" {
                static mut environ: *const *const c_char;
            }
            unsafe { Self::from_raw(environ) }
        }

        /// Lends each entry as `KEY=VALUE` bytes into fixed caller storage for child inheritance.
        ///
        /// # Errors
        ///
        /// Returns the entry count the environment holds, plus one, when `output` cannot hold every entry.
        pub fn collect(&self, output: &mut [&'environment [u8]]) -> Result<usize, usize> {
            if self.entries.is_null() {
                return Ok(0);
            }
            let mut count = 0;
            loop {
                let entry = unsafe { *self.entries.add(count) };
                if entry.is_null() {
                    return Ok(count);
                }
                if count == output.len() {
                    return Err(count + 1);
                }
                output[count] = unsafe { CStr::from_ptr(entry) }.to_bytes();
                count += 1;
            }
        }

        /// Lends the bytes after `KEY=`, preserving empty-present versus absent.
        #[must_use]
        pub fn get(&self, key: &[u8]) -> Option<&'environment [u8]> {
            if self.entries.is_null() || key.is_empty() {
                return None;
            }
            let mut index = 0;
            loop {
                let entry = unsafe { *self.entries.add(index) };
                if entry.is_null() {
                    return None;
                }
                let bytes: &'environment [u8] = unsafe { CStr::from_ptr(entry) }.to_bytes();
                if let Some(value) = bytes
                    .strip_prefix(key)
                    .and_then(|suffix| suffix.strip_prefix(b"="))
                {
                    return Some(value);
                }
                index += 1;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use core::ffi::{CStr, c_char};
        use core::ptr;

        use super::Environment;

        fn same_presence(left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
            left.is_some() == right.is_some()
        }

        #[test]
        fn c52_unix_lends_three_runtime_values_without_copying() {
            temp_env::with_vars(
                [
                    ("CARGO_VAL_JOBS", Some("2")),
                    ("CARGO_VAL_COLOR", Some("true")),
                    ("NO_COLOR", Some("")),
                ],
                || {
                    let environment = unsafe { Environment::current() };
                    let expected = [
                        (b"CARGO_VAL_JOBS".as_slice(), b"2".as_slice()),
                        (b"CARGO_VAL_COLOR".as_slice(), b"true".as_slice()),
                        (b"NO_COLOR".as_slice(), b"".as_slice()),
                    ];
                    let mut recognized_keys = 0;
                    let mut value_copies = 0;
                    for (key, bytes) in expected {
                        let value = environment.get(key).expect("fixture key is present");
                        assert_eq!(value, bytes);
                        let key_c = std::ffi::CString::new(key).expect("fixture key holds no nul");
                        let direct = unsafe { libc::getenv(key_c.as_ptr()) };
                        assert!(!direct.is_null(), "native getenv finds fixture key");
                        value_copies += usize::from(value.as_ptr() != direct.cast::<u8>());
                        recognized_keys += 1;
                    }
                    assert_eq!(recognized_keys, 3);
                    assert_eq!(value_copies, 0);
                    assert!(same_presence(environment.get(b"NO_COLOR"), Some(b"")));
                    std::println!(
                        "{{\"recognized_keys\":{recognized_keys},\"empty_no_color_present\":1,\"value_copies\":{value_copies}}}"
                    );
                },
            );
        }

        #[test]
        fn c52_unix_rejects_null_for_present_empty_no_color() {
            let present_entries: [*const c_char; 2] = [c"NO_COLOR=".as_ptr(), ptr::null()];
            let absent_entries: [*const c_char; 1] = [ptr::null()];
            let present = unsafe { Environment::from_raw(present_entries.as_ptr()) };
            let absent = unsafe { Environment::from_raw(absent_entries.as_ptr()) };
            let present_value = present.get(b"NO_COLOR");
            let missing_value = absent.get(b"NO_COLOR");
            assert_eq!(present_value, Some(b"".as_slice()));
            assert_eq!(missing_value, None);
            assert!(same_presence(present_value, Some(b"")));
            assert!(!same_presence(present_value, missing_value));
            let expected_pointer = unsafe { CStr::from_ptr(present_entries[0]) }
                .to_bytes()
                .as_ptr()
                .wrapping_add(b"NO_COLOR=".len());
            assert_eq!(
                present_value.expect("empty value is present").as_ptr(),
                expected_pointer
            );
            std::println!(
                "{{\"empty_no_color_present\":1,\"null_value_control_rejected\":1,\"value_copies\":0}}"
            );
        }
    }
}
#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
pub use environment::*;
