const DECIMAL_DIGITS: [u8; 10] = *b"0123456789";

/// Fixed-capacity text assembled without allocation; text past the capacity is cut.
#[derive(Debug)]
pub struct Line<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    length: usize,
}

impl<const CAPACITY: usize> Line<CAPACITY> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
        }
    }

    pub fn text(&mut self, text: &[u8]) {
        let room = self.bytes.len() - self.length;
        let taken = text.len().min(room);
        self.bytes[self.length..self.length + taken].copy_from_slice(&text[..taken]);
        self.length += taken;
    }

    pub fn clear(&mut self) {
        self.length = 0;
    }

    pub fn number(&mut self, value: usize) {
        let mut digits = [0; 20];
        let mut start = digits.len();
        let mut remaining = value;
        loop {
            start -= 1;
            digits[start] = DECIMAL_DIGITS[remaining % 10];
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        self.text(&digits[start..]);
    }

    /// The assembled bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl<const CAPACITY: usize> Default for Line<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
