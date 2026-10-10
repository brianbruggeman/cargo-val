//! Caller-owned UTF-16 command-line and environment storage for `CreateProcessW`.

use core::cmp::Ordering;

use crate::ExecutablePath;
use crate::test_intent::ExactTestIntent;

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

/// Lowers one exact-test intent and environment overlay into caller-owned UTF-16 buffers.
///
/// The environment ordering slice holds indexes into `inherited_environment`
/// followed by `child_overrides`. All capacity errors are checked before either
/// output buffer is changed.
pub fn lower_windows_arguments<'storage, 'intent>(
    intent: &ExactTestIntent<'_, '_, 'intent>,
    inherited_environment: &[&str],
    child_overrides: &[&str],
    command_line: &'storage mut [u16],
    environment_block: &'storage mut [u16],
    environment_order: &mut [usize],
) -> Result<WindowsArguments<'storage>, WindowsArgumentsError> {
    let executable = match intent.executable() {
        ExecutablePath::Borrowed(path) | ExecutablePath::Scratch(path) => path,
    };
    let arguments = [executable, "--exact", intent.test_name()];
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

    let mut command_offset = 0;
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            command_line[command_offset] = b' ' as u16;
            command_offset += 1;
        }
        command_offset += write_quoted_argument(argument, &mut command_line[command_offset..]);
    }
    command_line[command_offset] = 0;
    let mut environment_offset = 0;
    for index in environment_order.iter().take(order_count) {
        let entry = environment_entry(*index, inherited_environment, child_overrides);
        for unit in entry.encode_utf16() {
            environment_block[environment_offset] = unit;
            environment_offset += 1;
        }
        environment_block[environment_offset] = 0;
        environment_offset += 1;
    }
    environment_block[environment_offset] = 0;
    if order_count == 0 {
        environment_block[environment_offset + 1] = 0;
    }

    Ok(WindowsArguments {
        command_line: &mut command_line[..command_offset + 1],
        environment_block: &mut environment_block[..required_environment_units],
    })
}

fn quoted_argument_len(argument: &str) -> usize {
    let quote = argument.is_empty()
        || argument
            .encode_utf16()
            .any(|unit| matches!(unit, 0x20 | 0x09 | 0x22));
    let mut length = usize::from(quote) * 2;
    let mut backslashes = 0;
    for unit in argument.encode_utf16() {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else {
            length += backslashes;
            if unit == b'"' as u16 {
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
        output[offset] = b'"' as u16;
        offset += 1;
    }
    let mut backslashes = 0;
    for unit in argument.encode_utf16() {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            for _ in 0..(backslashes * 2 + 1) {
                output[offset] = b'\\' as u16;
                offset += 1;
            }
            output[offset] = unit;
            offset += 1;
        } else {
            for _ in 0..backslashes {
                output[offset] = b'\\' as u16;
                offset += 1;
            }
            output[offset] = unit;
            offset += 1;
        }
        backslashes = 0;
    }
    let trailing_backslashes = if quote { backslashes * 2 } else { backslashes };
    for _ in 0..trailing_backslashes {
        output[offset] = b'\\' as u16;
        offset += 1;
    }
    if quote {
        output[offset] = b'"' as u16;
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
    use proxima_primitives::pipe::Pipe;

    use super::{WindowsArgumentsError, lower_windows_arguments};
    use crate::ExecutablePath;
    use crate::test_intent::{ExactTestIntentInput, ExactTestIntentPipe};

    fn exact_intent<'name>(
        executable: &'static str,
        test_name: &'name str,
    ) -> crate::test_intent::ExactTestIntent<'static, 'static, 'name> {
        let pipe = ExactTestIntentPipe::new();
        let future = pipe.call(ExactTestIntentInput {
            executable: ExecutablePath::Borrowed(executable),
            test_name,
        });
        let mut future = core::pin::pin!(future);
        let mut context = core::task::Context::from_waker(core::task::Waker::noop());
        match core::future::Future::poll(future.as_mut(), &mut context) {
            core::task::Poll::Ready(Ok(intent)) => intent,
            core::task::Poll::Ready(Err(error)) => match error {},
            core::task::Poll::Pending => panic!("intent Pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn c43_quotes_executable_and_escapes_trailing_backslashes() {
        let intent = exact_intent("C:\\target dir\\fixture.exe", "alpha \\");
        let mut command_line = [0xA5; 128];
        let mut environment_block = [0xA5; 2];
        let mut environment_order = [];
        let (result, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
        });
        let mut lowered = result.expect("caller buffers fit command line and empty environment");
        let command_text = std::string::String::from_utf16(&*lowered.command_line())
            .expect("command line is utf-16");
        assert!(command_text.starts_with("\"C:\\target dir\\fixture.exe\" --exact \"alpha "));
        let command_units = lowered.command_line();
        assert_eq!(
            &command_units[command_units.len() - 4..],
            &[b'\\' as u16, b'\\' as u16, b'"' as u16, 0]
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
        let (result, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
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
}
