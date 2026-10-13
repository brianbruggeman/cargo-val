//! Glob matcher for filterset `#glob` and the default `package()` / `binary()` strategy.

/// Why a glob pattern was refused, with the byte offset inside the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobFault {
    /// A `[` with no closing `]`.
    UnclosedClass { offset: usize },
    /// `{a,b}` alternation, which this matcher does not provide.
    Alternation { offset: usize },
}

/// Checks that `pattern` only uses constructs [`glob_matches`] implements.
///
/// # Errors
///
/// Returns [`GlobFault`] for an unclosed `[` class or a `{` alternation.
pub fn glob_validate(pattern: &[u8]) -> Result<(), GlobFault> {
    let mut offset = 0;
    while offset < pattern.len() {
        match pattern[offset] {
            b'{' | b'}' => return Err(GlobFault::Alternation { offset }),
            b'[' => {
                offset = class_end(pattern, offset).ok_or(GlobFault::UnclosedClass { offset })?;
            }
            _ => {}
        }
        offset += 1;
    }
    Ok(())
}

// index of the `]` closing the class opened at `open`; a `]` right after `[` or `[!` is a member
fn class_end(pattern: &[u8], open: usize) -> Option<usize> {
    let mut cursor = open + 1;
    if matches!(pattern.get(cursor), Some(b'!' | b'^')) {
        cursor += 1;
    }
    if pattern.get(cursor) == Some(&b']') {
        cursor += 1;
    }
    pattern[cursor..]
        .iter()
        .position(|byte| *byte == b']')
        .map(|position| cursor + position)
}

fn class_admits(class: &[u8], byte: u8) -> bool {
    let (negated, members) = match class.first() {
        Some(b'!' | b'^') => (true, &class[1..]),
        _ => (false, class),
    };
    let mut index = 0;
    let mut found = false;
    while index < members.len() {
        let low = members[index];
        if members.get(index + 1) == Some(&b'-') && index + 2 < members.len() {
            found |= low <= byte && byte <= members[index + 2];
            index += 3;
        } else {
            found |= low == byte;
            index += 1;
        }
    }
    found != negated
}

/// Whether the whole of `text` matches `pattern`; `*` and `**` span any bytes, `?` one byte, `[..]` / `[!..]` a class.
#[must_use]
pub fn glob_matches(pattern: &[u8], text: &[u8]) -> bool {
    let mut pattern_at = 0;
    let mut text_at = 0;
    let mut resume: Option<(usize, usize)> = None;
    while text_at < text.len() {
        let advanced = match pattern.get(pattern_at) {
            Some(b'*') => {
                pattern_at += 1;
                resume = Some((pattern_at, text_at));
                continue;
            }
            Some(b'?') => Some((pattern_at + 1, 1)),
            Some(b'[') => class_end(pattern, pattern_at).and_then(|close| {
                class_admits(&pattern[pattern_at + 1..close], text[text_at])
                    .then_some((close + 1, 1))
            }),
            Some(literal) => (*literal == text[text_at]).then_some((pattern_at + 1, 1)),
            None => None,
        };
        match (advanced, resume) {
            (Some((next, step)), _) => {
                pattern_at = next;
                text_at += step;
            }
            (None, Some((star_pattern, star_text))) => {
                pattern_at = star_pattern;
                text_at = star_text + 1;
                resume = Some((star_pattern, star_text + 1));
            }
            (None, None) => return false,
        }
    }
    pattern[pattern_at..].iter().all(|byte| *byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::{GlobFault, glob_matches, glob_validate};

    #[proxima::test]
    async fn glob_semantics_follow_globset_defaults() {
        let cases: [(&str, &str, bool); 16] = [
            ("proxima-core", "proxima-core", true),
            ("proxima-core", "proxima-core-extra", false),
            ("proxima-*", "proxima-core", true),
            ("proxima-*", "proxima-", true),
            ("*core", "proxima-core", true),
            ("*", "", true),
            ("?", "", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("cap[a-z]*", "capability_matrix", true),
            ("cap[!a-z]*", "capability_matrix", false),
            ("[*]", "*", true),
            ("[*]", "a", false),
            ("a**b", "a::x::b", true),
            ("*::tests::*", "bind::tests::alpha", true),
            ("*::tests::*", "bind::testsx::alpha", false),
        ];
        let mut checked = 0;
        for (pattern, text, expected) in cases {
            assert_eq!(
                glob_matches(pattern.as_bytes(), text.as_bytes()),
                expected,
                "{pattern} against {text}"
            );
            checked += 1;
        }
        assert_eq!(checked, cases.len());
    }

    #[proxima::test]
    async fn validation_refuses_what_the_matcher_cannot_honor() {
        assert_eq!(glob_validate(b"a[bc"), Err(GlobFault::UnclosedClass { offset: 1 }));
        assert_eq!(glob_validate(b"x{a,b}"), Err(GlobFault::Alternation { offset: 1 }));
        assert_eq!(glob_validate(b"[]a]*"), Ok(()));
        assert_eq!(glob_validate(b"plain-*"), Ok(()));
    }
}
