//! UTF-8 helpers for reading text in pieces.

use std::str;

/// The bytes are not UTF-8, and no further bytes can make them so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// Returns the longest prefix of `bytes` that ends on a character boundary, leaving out a
/// final character that is incomplete but could still be completed by the bytes that follow.
///
/// Raises [`Invalid`] if `bytes` holds invalid UTF-8 anywhere else.
pub fn complete_prefix(bytes: &[u8]) -> Result<&str, Invalid> {
    match str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(err) if err.error_len().is_none() => {
            // SAFETY: `from_utf8` validated everything before `valid_up_to`.
            Ok(unsafe { str::from_utf8_unchecked(&bytes[..err.valid_up_to()]) })
        }
        Err(_) => Err(Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_text_is_whole() {
        assert_eq!(complete_prefix("aé".as_bytes()), Ok("aé"));
    }

    #[test]
    fn incomplete_final_character_is_left_out() {
        let bytes = "aé".as_bytes();
        assert_eq!(complete_prefix(&bytes[..2]), Ok("a"));
        assert_eq!(complete_prefix(&bytes[1..2]), Ok(""));
    }

    #[test]
    fn invalid_bytes_raise() {
        assert_eq!(complete_prefix(b"a\xffb"), Err(Invalid));
        assert_eq!(complete_prefix(b"\xc3a"), Err(Invalid));
    }
}
