//! Unix edge for lowering argv, envp, and process environment, plus the host path conventions.

use core::ffi::CStr;

#[cfg(target_os = "macos")]
pub const DYLIB_PATH_KEY: &str = "DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
pub const DYLIB_PATH_KEY: &str = "LD_LIBRARY_PATH";

#[cfg(target_os = "macos")]
pub const DYLIB_PATH_KEY_C: &CStr = c"DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
pub const DYLIB_PATH_KEY_C: &CStr = c"LD_LIBRARY_PATH";

pub const PATH_SEPARATOR: u8 = b':';
pub const MAIN_SEPARATOR: u8 = b'/';
pub const DEPS_SUFFIX: &str = "/deps";

#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
mod lowering {
    //! Caller-owned Unix argv and environment storage for one borrowed test intent.

    use core::ffi::{CStr, c_char};

    use crate::ExecutablePath;
    use crate::test_intent::ExactTestIntent;

    /// Bounded failures while lowering a borrowed intent for `posix_spawn`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UnixArgumentsError {
        /// The executable or exact-test arguments contain an interior NUL byte.
        InteriorNul,
        /// The caller's byte storage cannot hold every argument and its terminator.
        ArgumentStorageTooSmall { required: usize, available: usize },
        /// The caller's pointer storage cannot hold argv and its null sentinel.
        ArgumentPointersTooSmall { required: usize, available: usize },
        /// An environment entry has no non-empty key followed by `=`.
        InvalidEnvironmentEntry,
        /// The caller's pointer storage cannot hold the merged environment and sentinel.
        EnvironmentPointersTooSmall { required: usize, available: usize },
    }

    /// Borrowed C pointer arrays backed by caller-owned pointer and byte storage.
    #[derive(Debug, PartialEq, Eq)]
    pub struct UnixArguments<'storage> {
        argument_bytes: &'storage [u8],
        argv: &'storage [*const c_char],
        envp: &'storage [*const c_char],
    }

    impl UnixArguments<'_> {
        /// Returns the C strings in the argv backing storage without copying them.
        #[must_use]
        pub fn argument_bytes(&self) -> &[u8] {
            self.argument_bytes
        }

        /// Returns argv including its trailing null pointer.
        #[must_use]
        pub fn argv(&self) -> &[*const c_char] {
            self.argv
        }

        /// Returns the inherited environment overlaid with child-only entries.
        #[must_use]
        pub fn envp(&self) -> &[*const c_char] {
            self.envp
        }
    }

    /// Copies one borrowed test intent into caller-owned C-string storage.
    ///
    /// The returned pointers remain valid while `argument_bytes`, `argument_pointers`,
    /// `environment_pointers`, and all input `CStr`s remain alive. No heap storage is used.
    ///
    /// # Errors
    ///
    /// Returns [`UnixArgumentsError`] for interior NUL bytes, malformed environment entries, or caller storage that is too small.
    pub fn lower_unix_arguments<'storage>(
        intent: &ExactTestIntent<'_, '_, '_>,
        inherited_environment: &[&'storage CStr],
        child_overrides: &[&'storage CStr],
        harness_arguments: &[&[u8]],
        argument_bytes: &'storage mut [u8],
        argument_pointers: &'storage mut [*const c_char],
        environment_pointers: &'storage mut [*const c_char],
    ) -> Result<UnixArguments<'storage>, UnixArgumentsError> {
        let executable = match intent.executable() {
            ExecutablePath::Borrowed(path) | ExecutablePath::Scratch(path) => path.as_bytes(),
        };
        let arguments = [b"--exact".as_slice(), intent.test_name().as_bytes()];
        let mut required_argument_bytes = executable.len() + 1;
        for argument in arguments {
            if argument.contains(&0) {
                return Err(UnixArgumentsError::InteriorNul);
            }
            required_argument_bytes += argument.len() + 1;
        }
        for argument in harness_arguments {
            if argument.contains(&0) {
                return Err(UnixArgumentsError::InteriorNul);
            }
            required_argument_bytes += argument.len() + 1;
        }
        if executable.contains(&0) {
            return Err(UnixArgumentsError::InteriorNul);
        }
        if argument_bytes.len() < required_argument_bytes {
            return Err(UnixArgumentsError::ArgumentStorageTooSmall {
                required: required_argument_bytes,
                available: argument_bytes.len(),
            });
        }
        let required_argument_pointers = arguments.len() + harness_arguments.len() + 2;
        if argument_pointers.len() < required_argument_pointers {
            return Err(UnixArgumentsError::ArgumentPointersTooSmall {
                required: required_argument_pointers,
                available: argument_pointers.len(),
            });
        }

        let required_environment_pointers = required_environment_pointers(
            inherited_environment,
            child_overrides,
            environment_pointers,
        )?;

        let mut byte_offset = 0;
        let mut pointer_offset = 0;
        for argument in [executable, arguments[0], arguments[1]]
            .into_iter()
            .chain(harness_arguments.iter().copied())
        {
            argument_pointers[pointer_offset] = argument_bytes[byte_offset..].as_ptr().cast();
            argument_bytes[byte_offset..byte_offset + argument.len()].copy_from_slice(argument);
            byte_offset += argument.len();
            argument_bytes[byte_offset] = 0;
            byte_offset += 1;
            pointer_offset += 1;
        }
        argument_pointers[pointer_offset] = core::ptr::null();

        write_environment_pointers(inherited_environment, child_overrides, environment_pointers)?;

        Ok(UnixArguments {
            argument_bytes: &argument_bytes[..byte_offset],
            argv: &argument_pointers[..required_argument_pointers],
            envp: &environment_pointers[..required_environment_pointers],
        })
    }

    /// Merges inherited entries with child-only overrides into caller pointer storage.
    ///
    /// An override replaces any inherited entry with the same key. The returned
    /// count includes the trailing null sentinel.
    ///
    /// # Errors
    ///
    /// Returns [`UnixArgumentsError`] for malformed entries or pointer storage that is too small.
    pub fn merge_environment(
        inherited_environment: &[&CStr],
        child_overrides: &[&CStr],
        environment_pointers: &mut [*const c_char],
    ) -> Result<usize, UnixArgumentsError> {
        let required = required_environment_pointers(
            inherited_environment,
            child_overrides,
            environment_pointers,
        )?;
        write_environment_pointers(inherited_environment, child_overrides, environment_pointers)?;
        Ok(required)
    }

    fn required_environment_pointers(
        inherited_environment: &[&CStr],
        child_overrides: &[&CStr],
        environment_pointers: &[*const c_char],
    ) -> Result<usize, UnixArgumentsError> {
        for entry in inherited_environment.iter().chain(child_overrides) {
            environment_key(entry)?;
        }
        let mut environment_count = 0;
        for inherited in inherited_environment {
            if !child_overrides.iter().any(|override_entry| {
                environment_key(override_entry).ok() == environment_key(inherited).ok()
            }) {
                environment_count += 1;
            }
        }
        environment_count += child_overrides.len();
        let required = environment_count + 1;
        if environment_pointers.len() < required {
            return Err(UnixArgumentsError::EnvironmentPointersTooSmall {
                required,
                available: environment_pointers.len(),
            });
        }
        Ok(required)
    }

    fn write_environment_pointers(
        inherited_environment: &[&CStr],
        child_overrides: &[&CStr],
        environment_pointers: &mut [*const c_char],
    ) -> Result<(), UnixArgumentsError> {
        let mut environment_offset = 0;
        for inherited in inherited_environment {
            let inherited_key = environment_key(inherited)?;
            let overridden = child_overrides.iter().any(|override_entry| {
                environment_key(override_entry)
                    .is_ok_and(|override_key| override_key == inherited_key)
            });
            if !overridden {
                environment_pointers[environment_offset] = inherited.as_ptr();
                environment_offset += 1;
            }
        }
        for override_entry in child_overrides {
            environment_pointers[environment_offset] = override_entry.as_ptr();
            environment_offset += 1;
        }
        environment_pointers[environment_offset] = core::ptr::null();
        Ok(())
    }

    fn environment_key(entry: &CStr) -> Result<&[u8], UnixArgumentsError> {
        let bytes = entry.to_bytes();
        let Some(separator) = bytes.iter().position(|byte| *byte == b'=') else {
            return Err(UnixArgumentsError::InvalidEnvironmentEntry);
        };
        if separator == 0 {
            return Err(UnixArgumentsError::InvalidEnvironmentEntry);
        }
        Ok(&bytes[..separator])
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::CStr;

        use super::{UnixArgumentsError, lower_unix_arguments};
        use crate::ExecutablePath;

        fn c_string(bytes: &'static [u8]) -> &'static CStr {
            CStr::from_bytes_with_nul(bytes).expect("fixture is a nul-terminated C string")
        }

        fn exact_intent(
            test_name: &str,
        ) -> crate::test_intent::ExactTestIntent<'static, 'static, '_> {
            crate::test_intent::ExactTestIntent {
                executable: ExecutablePath::Borrowed("target/debug/deps/fixture"),
                test_name,
            }
        }

        #[test]
        fn c37_lowers_exact_argv_and_child_environment_into_caller_storage() {
            let inherited_environment = [c_string(b"PARENT=kept\0"), c_string(b"CHILD=old\0")];
            let child_overrides = [c_string(b"CHILD=override\0")];
            let intent = exact_intent("alpha");
            let mut argument_bytes = [0; 64];
            let mut argument_pointers = [std::ptr::null(); 4];
            let mut environment_pointers = [std::ptr::null(); 3];

            let (lowered_result, observed_allocations) =
                crate::test_alloc::measure_allocations(|| {
                    lower_unix_arguments(
                        &intent,
                        &inherited_environment,
                        &child_overrides,
                        &[],
                        &mut argument_bytes,
                        &mut argument_pointers,
                        &mut environment_pointers,
                    )
                });
            let lowered = lowered_result.expect("caller storage fits the launch data");

            assert_eq!(lowered.argv().len(), 4);
            assert_eq!(lowered.envp().len(), 3);
            let lowered_bytes = lowered.argument_bytes();
            assert_eq!(&lowered_bytes[..26], b"target/debug/deps/fixture\0");
            assert_eq!(&lowered_bytes[26..34], b"--exact\0");
            assert_eq!(&lowered_bytes[34..40], b"alpha\0");
            assert_eq!(lowered.argv()[0], lowered_bytes.as_ptr().cast());
            assert_eq!(lowered.argv()[1], lowered_bytes[26..].as_ptr().cast());
            assert_eq!(lowered.argv()[2], lowered_bytes[34..].as_ptr().cast());
            assert_eq!(lowered.envp()[0], inherited_environment[0].as_ptr());
            assert_eq!(lowered.envp()[1], child_overrides[0].as_ptr());
            assert!(lowered.argv()[3].is_null());
            assert!(lowered.envp()[2].is_null());
            assert_eq!(observed_allocations, 0);
            std::println!(
                "{{\"inherited_entries\":1,\"child_overrides\":1,\"argv_pairs\":1,\"allocations\":{observed_allocations}}}"
            );
        }

        #[test]
        fn c52_forwards_ignored_flag_after_the_exact_test_name_without_allocating() {
            let inherited_environment: [&CStr; 0] = [];
            let child_overrides: [&CStr; 0] = [];
            let harness_arguments = [b"--ignored".as_slice()];
            let intent = exact_intent("fixture::ignored");
            let mut argument_bytes = [0; 128];
            let mut argument_pointers = [std::ptr::null(); 5];
            let mut environment_pointers = [std::ptr::null(); 1];

            let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                lower_unix_arguments(
                    &intent,
                    &inherited_environment,
                    &child_overrides,
                    &harness_arguments,
                    &mut argument_bytes,
                    &mut argument_pointers,
                    &mut environment_pointers,
                )
            });
            let lowered = result.expect("fixed storage fits the ignored flag");
            let harness_offset = b"target/debug/deps/fixture\0".len()
                + b"--exact\0".len()
                + intent.test_name().len()
                + 1;

            assert_eq!(lowered.argv().len(), 5);
            assert_eq!(
                lowered.argv()[3],
                lowered.argument_bytes()[harness_offset..].as_ptr().cast()
            );
            assert_eq!(
                &lowered.argument_bytes()[harness_offset..harness_offset + b"--ignored\0".len()],
                b"--ignored\0"
            );
            assert!(lowered.argv()[4].is_null());
            assert_eq!(allocations, 0);
            std::println!(
                "{{\"exact_name_before_harness_flag\":1,\"ignored_flag_bytes\":9,\"allocations\":{allocations}}}"
            );
        }

        #[test]
        fn c37_rejects_one_byte_short_argv_storage_before_writing() {
            let intent = exact_intent("alpha");
            let mut argument_bytes = [0xA5; 39];
            let original_bytes = argument_bytes;
            let mut argument_pointers = [std::ptr::null(); 4];
            let original_argument_pointers = argument_pointers;
            let mut environment_pointers = [std::ptr::null(); 1];
            let original_environment_pointers = environment_pointers;

            let result = lower_unix_arguments(
                &intent,
                &[],
                &[],
                &[],
                &mut argument_bytes,
                &mut argument_pointers,
                &mut environment_pointers,
            );

            assert_eq!(
                result,
                Err(UnixArgumentsError::ArgumentStorageTooSmall {
                    required: 40,
                    available: 39,
                })
            );
            assert_eq!(argument_bytes, original_bytes);
            assert_eq!(argument_pointers, original_argument_pointers);
            assert_eq!(environment_pointers, original_environment_pointers);
            std::println!("{{\"truncated_argv_control_rejected\":1,\"storage_unchanged\":1}}");
        }

        #[test]
        fn c37_rejects_an_interior_nul_in_the_test_name() {
            let intent = exact_intent("al\0pha");
            let mut argument_bytes = [0xA5; 64];
            let original_bytes = argument_bytes;
            let mut argument_pointers = [std::ptr::null(); 4];
            let original_argument_pointers = argument_pointers;
            let mut environment_pointers = [std::ptr::null(); 1];
            let original_environment_pointers = environment_pointers;

            let result = lower_unix_arguments(
                &intent,
                &[],
                &[],
                &[],
                &mut argument_bytes,
                &mut argument_pointers,
                &mut environment_pointers,
            );

            assert_eq!(result, Err(UnixArgumentsError::InteriorNul));
            assert_eq!(argument_bytes, original_bytes);
            assert_eq!(argument_pointers, original_argument_pointers);
            assert_eq!(environment_pointers, original_environment_pointers);
            std::println!("{{\"interior_nul_control_rejected\":1,\"storage_unchanged\":1}}");
        }

        #[test]
        fn c37_rejects_one_byte_short_argv_pointer_storage_before_writing() {
            let intent = exact_intent("alpha");
            let mut argument_bytes = [0; 64];
            let original_bytes = argument_bytes;
            let mut argument_pointers = [std::ptr::null(); 3];
            let original_argument_pointers = argument_pointers;
            let mut environment_pointers = [std::ptr::null(); 2];
            let original_environment_pointers = environment_pointers;

            let result = lower_unix_arguments(
                &intent,
                &[],
                &[],
                &[],
                &mut argument_bytes,
                &mut argument_pointers,
                &mut environment_pointers,
            );

            assert_eq!(
                result,
                Err(UnixArgumentsError::ArgumentPointersTooSmall {
                    required: 4,
                    available: 3,
                })
            );
            assert_eq!(argument_bytes, original_bytes);
            assert_eq!(argument_pointers, original_argument_pointers);
            assert_eq!(environment_pointers, original_environment_pointers);
            std::println!("{{\"short_argv_pointer_control_rejected\":1,\"storage_unchanged\":1}}");
        }
    }
}
#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
pub use lowering::*;

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
mod environment {
    //! Borrowed Unix process environment values for the runtime settings boundary.

    use core::ffi::{CStr, c_char};
    use core::marker::PhantomData;

    /// A borrowed, null-terminated Unix environment pointer table.
    pub struct UnixEnvironment<'environment> {
        entries: *const *const c_char,
        lifetime: PhantomData<&'environment CStr>,
    }

    impl<'environment> UnixEnvironment<'environment> {
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

        /// Copies C string references into fixed caller storage for child inheritance.
        ///
        /// # Errors
        ///
        /// Returns the entry count the environment holds, plus one, when `output` cannot hold every entry.
        pub fn collect(&self, output: &mut [&'environment CStr]) -> Result<usize, usize> {
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
                output[count] = unsafe { CStr::from_ptr(entry) };
                count += 1;
            }
        }

        /// Lends the bytes after `KEY=`, preserving empty-present versus absent.
        #[must_use]
        pub fn get(&self, key: &CStr) -> Option<&'environment [u8]> {
            if self.entries.is_null() || key.to_bytes().is_empty() {
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
                    .strip_prefix(key.to_bytes())
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

        use super::UnixEnvironment;

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
                    let environment = unsafe { UnixEnvironment::current() };
                    let expected = [
                        (c"CARGO_VAL_JOBS", b"2".as_slice()),
                        (c"CARGO_VAL_COLOR", b"true".as_slice()),
                        (c"NO_COLOR", b"".as_slice()),
                    ];
                    let mut recognized_keys = 0;
                    let mut value_copies = 0;
                    for (key, bytes) in expected {
                        let value = environment.get(key).expect("fixture key is present");
                        assert_eq!(value, bytes);
                        let direct = unsafe { libc::getenv(key.as_ptr()) };
                        assert!(!direct.is_null(), "native getenv finds fixture key");
                        value_copies += usize::from(value.as_ptr() != direct.cast::<u8>());
                        recognized_keys += 1;
                    }
                    assert_eq!(recognized_keys, 3);
                    assert_eq!(value_copies, 0);
                    assert!(same_presence(environment.get(c"NO_COLOR"), Some(b"")));
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
            let present = unsafe { UnixEnvironment::from_raw(present_entries.as_ptr()) };
            let absent = unsafe { UnixEnvironment::from_raw(absent_entries.as_ptr()) };
            let present_value = present.get(c"NO_COLOR");
            let missing_value = absent.get(c"NO_COLOR");
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
