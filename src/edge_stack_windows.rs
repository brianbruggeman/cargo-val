use crate::stack_capture::Unavailable;
use crate::stack_frames::Format;

/// The format a successful capture would hold; Windows never succeeds yet, so this only keeps the signature the same as the other platforms.
pub const FORMAT: Format = Format::Sample;

/// Windows has no stack capture yet; the minidump or stack-walk slice `S8w` adds it.
///
/// Same signature as the macOS and Linux captures, so the caller never branches on the OS: the answer is an explicit
/// [`Unavailable::Unsupported`], not an empty success.
///
/// # Errors
///
/// Always [`Unavailable::Unsupported`].
pub fn capture(_pid: u32, _buffer: &mut [u8]) -> Result<(usize, usize), Unavailable> {
    Err(Unavailable::Unsupported { follow_up: "S8w windows stack walk" })
}

#[cfg(test)]
mod tests {
    use super::capture;
    use crate::stack_capture::Unavailable;

    #[test]
    fn windows_capture_names_the_follow_up_slice() {
        let mut buffer = [0_u8; 16];

        let outcome = capture(4242, &mut buffer);

        assert_eq!(outcome, Err(Unavailable::Unsupported { follow_up: "S8w windows stack walk" }));
    }
}
