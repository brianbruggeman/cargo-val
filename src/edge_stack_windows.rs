use crate::stack_capture::{Capture, Unavailable};

/// Windows has no stack capture yet; the minidump or stack-walk slice `S8w` adds it.
///
/// Same signature as the macOS and Linux captures, so the caller never branches on the OS: the answer is an explicit
/// [`Unavailable::Unsupported`], not an empty success.
pub fn capture(_pid: u32, _buffer: &mut [u8]) -> Capture {
    Capture::Unavailable(Unavailable::Unsupported { follow_up: "S8w windows stack walk" })
}

#[cfg(test)]
mod tests {
    use super::capture;
    use crate::stack_capture::{Capture, Unavailable};

    #[test]
    fn windows_capture_names_the_follow_up_slice() {
        let mut buffer = [0_u8; 16];

        let outcome = capture(4242, &mut buffer);

        assert_eq!(outcome, Capture::Unavailable(Unavailable::Unsupported { follow_up: "S8w windows stack walk" }));
    }
}
