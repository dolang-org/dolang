//! The stream a typelib's spans are written to, in the order its content holds
//! them.
//!
//! Each span is written relative to the one before it, as words that the
//! serialization writes as varints. Its first word holds a kind in its low two
//! bits, then whether its length is written, then the kind's payload:
//!
//! - **Same line**: the payload is how far its start is past where the span
//!   before it ended, zigzag-coded. Nothing more is written.
//! - **New line**: the payload is how many lines it is past the span before it,
//!   zigzag-coded. A word follows: how far its column is past the column of the
//!   first span on the span before it's line, zigzag-coded.
//! - **Invalid**: there is no span, and the next is relative to the one before.
//!
//! A span's length follows, when written. A span that locates a name and is as
//! long as it in characters leaves its length out.
//!
//! So a name on the same line as the span before it is usually one byte, and one
//! starting a line is usually two.

use std::slice;

use crate::diag::Span;

const SAME_LINE: u32 = 0;
const NEW_LINE: u32 = 1;
const INVALID: u32 = 2;
const KIND: u32 = 3;
const LENGTH: u32 = 1 << 2;
const PAYLOAD_SHIFT: u32 = 3;

/// Where the last span was, which the next is written relative to
#[derive(Default)]
struct State {
    line: u32,
    /// Its column plus its length
    end: i64,
    /// The column of the first span on its line
    first: u32,
}

/// Writes spans to a stream
#[derive(Default)]
pub(super) struct Writer {
    words: Vec<u32>,
    state: State,
}

impl Writer {
    /// Write a span, of a name with text `name` if it locates one.
    pub(super) fn push(&mut self, span: Span, name: Option<&str>) {
        if span == Span::INVALID {
            self.words.push(INVALID);
            return;
        }
        let state = &mut self.state;
        let start = span.start();
        let (line, column, len) = (start.line_offset(), start.column_offset(), span.len());
        let written = match name {
            Some(name) => chars(name) != len,
            None => true,
        };
        let length = if written { LENGTH } else { 0 };
        if line == state.line {
            let gap = i64::from(column) - state.end;
            self.words.push(head(zag(gap), length | SAME_LINE));
        } else {
            let lines = i64::from(line) - i64::from(state.line);
            self.words.push(head(zag(lines), length | NEW_LINE));
            self.words
                .push(zag(i64::from(column) - i64::from(state.first)));
            state.first = column;
        }
        if written {
            self.words.push(len);
        }
        state.line = line;
        state.end = i64::from(column) + i64::from(len);
    }

    /// The stream written
    pub(super) fn finish(self) -> Vec<u32> {
        self.words
    }
}

/// Reads spans from a stream
pub(super) struct Reader<'a> {
    words: slice::Iter<'a, u32>,
    state: State,
}

impl<'a> Reader<'a> {
    pub(super) fn new(words: &'a [u32]) -> Self {
        Self {
            words: words.iter(),
            state: State::default(),
        }
    }

    fn word(&mut self) -> Result<u32, &'static str> {
        self.words
            .next()
            .copied()
            .ok_or("the span stream ends early")
    }

    /// Read a span, of a name with text `name` if it locates one.
    pub(super) fn next(&mut self, name: Option<&str>) -> Result<Span, &'static str> {
        let head = self.word()?;
        let payload = zig(head >> PAYLOAD_SHIFT);
        let (line, column) = match head & KIND {
            SAME_LINE => (self.state.line, self.state.end + payload),
            NEW_LINE => {
                let line = i64::from(self.state.line) + payload;
                let column = i64::from(self.state.first) + zig(self.word()?);
                (field(line)?, column)
            }
            INVALID if head == INVALID => return Ok(Span::INVALID),
            _ => return Err("a span is malformed"),
        };
        let column = field(column)?;
        let len = match (head & LENGTH != 0, name) {
            (true, _) => self.word()?,
            (false, Some(name)) => chars(name),
            (false, None) => return Err("a span's length is missing"),
        };
        let state = &mut self.state;
        if line != state.line {
            state.first = column;
        }
        state.line = line;
        state.end = i64::from(column) + i64::from(len);
        Ok(Span::new(line, column, len))
    }

    /// Check that every span was read.
    pub(super) fn finish(self) -> Result<(), &'static str> {
        match self.words.len() {
            0 => Ok(()),
            _ => Err("the span stream has words left over"),
        }
    }
}

fn head(payload: u32, flags: u32) -> u32 {
    payload << PAYLOAD_SHIFT | flags
}

/// A line or column, which is never negative
fn field(value: i64) -> Result<u32, &'static str> {
    u32::try_from(value).map_err(|_| "a span is out of range")
}

/// The length of a name, in characters
fn chars(name: &str) -> u32 {
    u32::try_from(name.chars().count()).unwrap_or(u32::MAX)
}

// After dolang-bytecode's varints. A delta between fields of a span fits in 29
// bits, so the payload's shift loses nothing.
fn zag(x: i64) -> u32 {
    ((x << 1) ^ (x >> (i64::BITS - 1))) as u32
}

fn zig(x: u32) -> i64 {
    i64::from(x >> 1) ^ -i64::from(x & 1)
}

#[cfg(test)]
mod tests {
    use super::{Reader, Writer, zag, zig};
    use crate::diag::Span;

    fn round_trip(spans: &[(Span, Option<&str>)]) -> Vec<u32> {
        let mut writer = Writer::default();
        for &(span, name) in spans {
            writer.push(span, name);
        }
        let words = writer.finish();
        let mut reader = Reader::new(&words);
        for &(span, name) in spans {
            assert_eq!(reader.next(name), Ok(span));
        }
        assert_eq!(reader.finish(), Ok(()));
        words
    }

    #[test]
    fn zigzag() {
        for x in [0, 1, -1, 2, -2, 1 << 20, -(1 << 20), 1 << 25] {
            assert_eq!(zig(zag(x)), x);
        }
        assert_eq!([zag(0), zag(-1), zag(1), zag(-2)], [0, 1, 2, 3]);
    }

    #[test]
    fn names_on_a_line_take_a_word() {
        // `def f a b`, then `    g` and `    h` on the lines after
        let words = round_trip(&[
            (Span::new(3, 4, 1), Some("f")),
            (Span::new(3, 6, 1), Some("a")),
            (Span::new(3, 8, 1), Some("b")),
            (Span::new(4, 4, 1), Some("g")),
            (Span::new(5, 4, 1), Some("h")),
        ]);
        // The first starts a line; each later one on it is a gap of one
        assert_eq!(words.len(), 2 + 1 + 1 + 2 + 2);
        assert!(words.iter().all(|&word| word < 0x80), "{words:?}");
        // The lines after it start where it did
        assert_eq!(words[5], 0);
        assert_eq!(words[7], 0);
    }

    #[test]
    fn lengths_are_written_unless_implied() {
        round_trip(&[
            // Characters, not bytes
            (Span::new(1, 2, 3), Some("é\tü")),
            (Span::new(1, 7, 2), Some("long")),
            (Span::new(1, 9, 5), None),
            (Span::new(1, 0, 0), None),
        ]);
    }

    #[test]
    fn spans_go_backwards() {
        round_trip(&[
            (Span::new(9, 10, 3), Some("abc")),
            // Starting inside the last, and before it on its line
            (Span::new(9, 11, 1), None),
            (Span::new(9, 0, 2), Some("ab")),
            (Span::new(2, 30, 2), Some("ab")),
            (Span::new(2, 1, 2), Some("ab")),
            (Span::new(0, 0, 2), Some("ab")),
        ]);
    }

    #[test]
    fn invalid_spans_leave_the_reference() {
        let words = round_trip(&[
            (Span::new(4, 2, 1), Some("x")),
            (Span::INVALID, Some("y")),
            (Span::INVALID, None),
            (Span::new(4, 4, 1), Some("z")),
        ]);
        assert_eq!(words[words.len() - 1], 2 << 3);
    }

    #[test]
    fn clamped_fields_round_trip() {
        round_trip(&[
            (Span::new(1 << 24, 1 << 20, 1 << 20), None),
            (Span::new(0, 0, 0), None),
            (Span::new(u32::MAX, 0, 1), Some("x")),
        ]);
    }

    #[test]
    fn malformed_streams_are_rejected() {
        let mut writer = Writer::default();
        writer.push(Span::new(5, 3, 2), None);
        let words = writer.finish();
        // Truncated
        let mut reader = Reader::new(&words[..words.len() - 1]);
        assert!(reader.next(None).is_err());
        // Left over
        let mut reader = Reader::new(&words);
        assert!(reader.next(None).is_ok());
        let mut extra = words.clone();
        extra.push(0);
        let mut reader = Reader::new(&extra);
        reader.next(None).unwrap();
        assert!(reader.finish().is_err());
        // A length implied by no name
        assert!(Reader::new(&[0]).next(None).is_err());
        // Before the first line or column
        assert!(Reader::new(&[1 << 3 | 1, 1]).next(Some("x")).is_err());
        assert!(Reader::new(&[1 << 3]).next(Some("x")).is_err());
        // An unknown kind, and an invalid span with flags
        assert!(Reader::new(&[3]).next(Some("x")).is_err());
        assert!(Reader::new(&[2 | 4]).next(Some("x")).is_err());
    }
}
