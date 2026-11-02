//! Source locations, shared by the compiler's diagnostics and bytecode debug info

use std::fmt::{self, Debug};

/// Source code position: a line, and a column within it
///
/// The column counts Unicode scalar values from the start of the line, a tab
/// counting as one, as editors do. Where a UTF-16 or display column is wanted, it
/// is computed from the line's text.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    line: u32,
    column: u32,
}

impl Pos {
    /// Construct a position from a zero-based line and column.
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Line (starting from 0)
    pub fn line_offset(&self) -> u32 {
        self.line
    }

    /// Column (starting from 0), in Unicode scalar values
    pub fn column_offset(&self) -> u32 {
        self.column
    }

    /// Line number (starting from 1)
    pub fn line_number(&self) -> u32 {
        self.line + 1
    }

    /// Column number (starting from 1), in Unicode scalar values
    pub fn column_number(&self) -> u32 {
        self.column + 1
    }
}

/// Source code span: a start position and a length
///
/// The length counts Unicode scalar values, as the column of [`Pos`] does,
/// including those of each line break a span crosses (`\r\n` is two). Where it
/// ends can therefore be found only with the source text.
///
/// Spans order by start, then by length. A line, column or length too large to
/// represent is clamped, which only shortens what is highlighted.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span(u64);

impl Span {
    const LINE_BITS: u32 = 24;
    const COLUMN_BITS: u32 = 20;
    const LEN_BITS: u32 = 20;

    /// Largest representable line
    pub const MAX_LINE: u32 = (1 << Self::LINE_BITS) - 1;
    /// Largest representable column
    pub const MAX_COLUMN: u32 = (1 << Self::COLUMN_BITS) - 1;
    /// Largest representable length
    pub const MAX_LEN: u32 = (1 << Self::LEN_BITS) - 1;

    /// Construct a span from a zero-based line and column and a length,
    /// clamping each field to its range.
    pub const fn new(line: u32, column: u32, len: u32) -> Self {
        const fn field(value: u32, max: u32) -> u64 {
            (if value > max { max } else { value }) as u64
        }
        Self(
            field(line, Self::MAX_LINE) << (Self::COLUMN_BITS + Self::LEN_BITS)
                | field(column, Self::MAX_COLUMN) << Self::LEN_BITS
                | field(len, Self::MAX_LEN),
        )
    }

    /// Start of span (inclusive)
    pub fn start(&self) -> Pos {
        let column = (self.0 >> Self::LEN_BITS) & ((1 << Self::COLUMN_BITS) - 1);
        Pos::new(
            (self.0 >> (Self::COLUMN_BITS + Self::LEN_BITS)) as u32,
            column as u32,
        )
    }

    /// Length, in Unicode scalar values
    pub fn len(&self) -> u32 {
        (self.0 & ((1 << Self::LEN_BITS) - 1)) as u32
    }

    /// Whether the span is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = self.start();
        write!(f, "{}:{}+{}", start.line, start.column, self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_pack() {
        let span = Span::new(70_000, 300, 12);
        assert_eq!(span.start(), Pos::new(70_000, 300));
        assert_eq!(span.len(), 12);
    }

    #[test]
    fn fields_saturate() {
        let span = Span::new(u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(span.start(), Pos::new(Span::MAX_LINE, Span::MAX_COLUMN));
        assert_eq!(span.len(), Span::MAX_LEN);
        assert_eq!(span, Span(u64::MAX));
        let span = Span::new(1, u32::MAX, 3);
        assert_eq!((span.start().line_offset(), span.len()), (1, 3));
    }

    #[test]
    fn spans_order_by_start_then_length() {
        let mut spans = [
            Span::new(1, 0, 1),
            Span::new(0, 5, 9),
            Span::new(0, 5, 2),
            Span::new(0, 6, 0),
        ];
        spans.sort();
        assert_eq!(
            spans,
            [
                Span::new(0, 5, 2),
                Span::new(0, 5, 9),
                Span::new(0, 6, 0),
                Span::new(1, 0, 1),
            ]
        );
    }
}
