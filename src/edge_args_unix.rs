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
    //! Owned Unix argv and envp for one child launch, lowered from byte slices.

    use core::ffi::c_char;
    use std::vec::Vec;

    /// Why launch data cannot be represented as C strings.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ArgumentsError {
        /// The program, an argument, or an environment entry contains a NUL byte.
        InteriorNul,
        /// An environment entry has no non-empty key followed by `=`.
        InvalidEnvironmentEntry,
    }

    /// NUL-terminated argv and envp pointer tables over one owned byte buffer.
    #[derive(Debug)]
    pub struct Arguments {
        bytes: Vec<u8>,
        argv: Vec<*const c_char>,
        envp: Vec<*const c_char>,
    }

    impl Arguments {
        /// Returns argv including its trailing null pointer.
        #[must_use]
        pub fn argv(&self) -> &[*const c_char] {
            &self.argv
        }

        /// Returns the inherited environment overlaid with child-only entries, including its trailing null pointer.
        #[must_use]
        pub fn envp(&self) -> &[*const c_char] {
            &self.envp
        }

        /// Returns every lowered string, each followed by its NUL.
        #[must_use]
        pub fn bytes(&self) -> &[u8] {
            &self.bytes
        }
    }

    /// Copies one launch into owned C-string storage.
    ///
    /// `arguments` follow the program name, which becomes `argv[0]`. An override replaces any
    /// inherited entry with the same key.
    ///
    /// # Errors
    ///
    /// Returns [`ArgumentsError`] for an interior NUL byte or an environment entry without a key.
    pub fn lower_arguments(
        program: &[u8],
        arguments: &[&[u8]],
        inherited: &[&[u8]],
        overrides: &[&[u8]],
    ) -> Result<Arguments, ArgumentsError> {
        for entry in inherited.iter().chain(overrides) {
            environment_key(entry)?;
        }
        let kept = inherited.iter().filter(|entry| !overridden(entry, overrides));
        let argument_strings = core::iter::once(program).chain(arguments.iter().copied());
        let environment_strings = kept.copied().chain(overrides.iter().copied());
        let mut bytes = Vec::new();
        let argv_offsets = write_strings(argument_strings, &mut bytes)?;
        let envp_offsets = write_strings(environment_strings, &mut bytes)?;
        Ok(Arguments {
            argv: pointers(&bytes, &argv_offsets),
            envp: pointers(&bytes, &envp_offsets),
            bytes,
        })
    }

    fn write_strings<'text>(
        strings: impl Iterator<Item = &'text [u8]>,
        bytes: &mut Vec<u8>,
    ) -> Result<Vec<usize>, ArgumentsError> {
        let mut offsets = Vec::new();
        for text in strings {
            if text.contains(&0) {
                return Err(ArgumentsError::InteriorNul);
            }
            offsets.push(bytes.len());
            bytes.extend_from_slice(text);
            bytes.push(0);
        }
        Ok(offsets)
    }

    fn pointers(bytes: &[u8], offsets: &[usize]) -> Vec<*const c_char> {
        offsets
            .iter()
            .map(|offset| bytes[*offset..].as_ptr().cast::<c_char>())
            .chain(core::iter::once(core::ptr::null()))
            .collect()
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

        use super::{ArgumentsError, lower_arguments};

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

        #[test]
        fn c37_lowers_exact_argv_and_child_environment() {
            let lowered = lower_arguments(
                b"target/debug/deps/fixture",
                &[b"--exact", b"alpha"],
                &[b"PARENT=kept", b"CHILD=old"],
                &[b"CHILD=override"],
            )
            .expect("launch data holds no NUL");

            assert_eq!(
                strings(lowered.argv()),
                ["target/debug/deps/fixture", "--exact", "alpha"]
            );
            assert_eq!(strings(lowered.envp()), ["PARENT=kept", "CHILD=override"]);
            assert!(lowered.argv()[3].is_null());
            assert!(lowered.envp()[2].is_null());
            assert_eq!(
                &lowered.bytes()[..26],
                b"target/debug/deps/fixture\0"
            );
            std::println!(
                "{{\"inherited_entries\":1,\"child_overrides\":1,\"argv_pairs\":1}}"
            );
        }

        #[test]
        fn c52_forwards_ignored_flag_after_the_exact_test_name() {
            let lowered = lower_arguments(
                b"target/debug/deps/fixture",
                &[b"--exact", b"fixture::ignored", b"--ignored"],
                &[],
                &[],
            )
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
        fn c37_rejects_an_interior_nul_in_the_test_name() {
            let result = lower_arguments(b"fixture", &[b"--exact", b"al\0pha"], &[], &[]);

            assert_eq!(result.unwrap_err(), ArgumentsError::InteriorNul);
            std::println!("{{\"interior_nul_control_rejected\":1}}");
        }

        #[test]
        fn c37_rejects_an_interior_nul_in_the_program() {
            let result = lower_arguments(b"fix\0ture", &[], &[], &[]);

            assert_eq!(result.unwrap_err(), ArgumentsError::InteriorNul);
        }

        #[test]
        fn c37_rejects_an_environment_entry_without_a_key() {
            let missing_separator = lower_arguments(b"fixture", &[], &[b"NOEQUALS"], &[]);
            let empty_key = lower_arguments(b"fixture", &[], &[], &[b"=value"]);

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
            let lowered = lower_arguments(
                b"fixture",
                &[],
                &[b"A=1", b"B=2", b"C=3"],
                &[b"B=9", b"D=4"],
            )
            .expect("launch data holds no NUL");

            assert_eq!(strings(lowered.envp()), ["A=1", "C=3", "B=9", "D=4"]);
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
