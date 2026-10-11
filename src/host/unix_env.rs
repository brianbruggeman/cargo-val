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
