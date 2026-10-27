//! Conversion between UTF-8 byte offsets and UTF-16 offsets, for editors that
//! count text in UTF-16 code units.

/// A text's offsets in both encodings.
pub struct Offsets {
    /// The UTF-16 offset of each byte's character, by byte, and of the end
    utf16: Box<[usize]>,
}

impl Offsets {
    pub fn new(text: &str) -> Self {
        let mut utf16 = vec![0; text.len() + 1];
        let mut offset = 0;
        for (byte, ch) in text.char_indices() {
            utf16[byte..byte + ch.len_utf8()].fill(offset);
            offset += ch.len_utf16();
        }
        utf16[text.len()] = offset;
        Self {
            utf16: utf16.into(),
        }
    }

    /// The UTF-16 offset of the character containing the byte at `byte`. A byte
    /// past the end is at the end.
    pub fn utf16(&self, byte: usize) -> usize {
        self.utf16[byte.min(self.utf16.len() - 1)]
    }

    /// The byte offset of the character containing the UTF-16 code unit at
    /// `utf16`, so one inside a surrogate pair is at its character's start. A
    /// code unit past the end is at the end.
    pub fn byte(&self, utf16: usize) -> usize {
        // Every byte of a character has its offset, so the last byte at or
        // before `utf16` is in its character, which starts at the first byte
        // with the same offset
        let last = self.utf16.partition_point(|&offset| offset <= utf16) - 1;
        let start = self.utf16[last];
        self.utf16.partition_point(|&offset| offset < start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_offsets_agree() {
        let offsets = Offsets::new("abc");
        for offset in 0..=3 {
            assert_eq!(offsets.utf16(offset), offset);
            assert_eq!(offsets.byte(offset), offset);
        }
    }

    #[test]
    fn multibyte_characters_are_one_code_unit() {
        // `é` is two bytes and one code unit
        let offsets = Offsets::new("aéb");
        assert_eq!(
            (0..=4).map(|byte| offsets.utf16(byte)).collect::<Vec<_>>(),
            [0, 1, 1, 2, 3]
        );
        assert_eq!(
            (0..=3).map(|utf16| offsets.byte(utf16)).collect::<Vec<_>>(),
            [0, 1, 3, 4]
        );
    }

    #[test]
    fn astral_characters_are_surrogate_pairs() {
        // `😀` is four bytes and two code units
        let offsets = Offsets::new("a😀b");
        assert_eq!(offsets.utf16(1), 1);
        assert_eq!(offsets.utf16(4), 1);
        assert_eq!(offsets.utf16(5), 3);
        assert_eq!(offsets.byte(1), 1);
        assert_eq!(offsets.byte(2), 1, "inside the pair is at its start");
        assert_eq!(offsets.byte(3), 5);
    }

    #[test]
    fn offsets_past_the_end_clamp() {
        let offsets = Offsets::new("é");
        assert_eq!(offsets.utf16(10), 1);
        assert_eq!(offsets.byte(10), 2);
        let empty = Offsets::new("");
        assert_eq!(empty.utf16(3), 0);
        assert_eq!(empty.byte(3), 0);
    }
}
