use std::fmt::{Debug, Display};

use crate::UnitId;

pub(crate) const CLASS_TOP_LEVEL: &str =
    "`class` and `@class` declarations are only allowed at module top level";

/// Diagnostic severity
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    /// Error
    ///
    /// Always fatal to compilation
    Error,
    /// Warning
    Warning,
}

impl Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
        }
    }
}

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
    pub(crate) fn new(line: u32, column: u32) -> Self {
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
/// ends can therefore be found only with the text: [`Unit::end`](crate::Unit::end)
/// gives the end position, and [`Unit::byte_range`](crate::Unit::byte_range) and
/// [`Unit::text`](crate::Unit::text) slice the source.
///
/// Spans order by start, then by length. A line, column or length too large to
/// represent is clamped, which only shortens what is highlighted.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span(u64);

impl Span {
    const LINE_BITS: u32 = 24;
    const COLUMN_BITS: u32 = 20;
    const LEN_BITS: u32 = 20;

    /// Construct a span, clamping each field to its range.
    pub(crate) fn new(line: u32, column: u32, len: u32) -> Self {
        let field = |value: u32, bits: u32| u64::from(value).min((1 << bits) - 1);
        Self(
            field(line, Self::LINE_BITS) << (Self::COLUMN_BITS + Self::LEN_BITS)
                | field(column, Self::COLUMN_BITS) << Self::LEN_BITS
                | field(len, Self::LEN_BITS),
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let start = self.start();
        write!(f, "{}:{}+{}", start.line, start.column, self.len())
    }
}

/// A diagnostic location, optionally qualified by a checked unit.
///
/// A location without a unit is local to the [`Unit`](crate::Unit) whose
/// diagnostics produced it. Type checker diagnostics name a unit in every
/// location.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceSpan {
    unit: Option<UnitId>,
    span: Span,
}

impl SourceSpan {
    pub(crate) fn new(unit: Option<UnitId>, span: Span) -> Self {
        Self { unit, span }
    }

    pub fn unit(&self) -> Option<UnitId> {
        self.unit
    }
    pub fn span(&self) -> Span {
        self.span
    }
    pub(crate) fn dup(&self) -> Self {
        self.clone()
    }
}

/// Compiler diagnostic
#[derive(Clone)]
pub struct Diag {
    severity: Severity,
    span: SourceSpan,
    message: String,
    annotations: Vec<Annotation>,
    notes: Vec<Note>,
    patches: Vec<Patch>,
}

/// Annotation kind
#[non_exhaustive]
#[derive(Clone)]
pub enum AnnotationKind {
    /// The primary cause of the diagnostic
    Primary,
    /// Additional context relevant to the diagnostic
    Context,
}

impl AnnotationKind {
    fn dup(&self) -> Self {
        match self {
            AnnotationKind::Primary => AnnotationKind::Primary,
            AnnotationKind::Context => AnnotationKind::Context,
        }
    }
}

/// Source annotation.
#[derive(Clone)]
pub struct Annotation {
    pub(crate) kind: AnnotationKind,
    pub(crate) span: SourceSpan,
    pub(crate) message: String,
}

impl Annotation {
    /// Kind of annotation
    pub fn kind(&self) -> AnnotationKind {
        self.kind.dup()
    }

    /// Span of annotation in source code
    pub fn span(&self) -> SourceSpan {
        self.span.dup()
    }

    /// Message
    pub fn message(&self) -> impl Display + '_ {
        self.message.as_str()
    }
}

/// Kind of note
#[non_exhaustive]
#[derive(Clone)]
pub enum NoteKind {
    /// Additional information
    Info,
    /// Help resolving the issue
    Help,
}

impl NoteKind {
    fn dup(&self) -> Self {
        match self {
            NoteKind::Help => NoteKind::Help,
            NoteKind::Info => NoteKind::Info,
        }
    }
}

/// Additional diagnostic note
#[derive(Clone)]
pub struct Note {
    pub(crate) kind: NoteKind,
    pub(crate) message: String,
}

impl Note {
    /// Kind of note
    pub fn kind(&self) -> NoteKind {
        self.kind.dup()
    }

    /// Message
    pub fn message(&self) -> impl Display + '_ {
        self.message.as_str()
    }
}

/// Suggested change to source code
#[derive(Clone)]
pub struct Patch {
    pub(crate) span: SourceSpan,
    pub(crate) message: String,
    pub(crate) sub: String,
}

impl Patch {
    /// Span in source code to change
    pub fn span(&self) -> SourceSpan {
        self.span.dup()
    }

    /// Text to substitute
    pub fn sub(&self) -> &str {
        &self.sub
    }

    /// Message
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Diag {
    pub(crate) fn new(
        severity: Severity,
        span: SourceSpan,
        message: impl Into<String>,
        annotations: impl Iterator<Item = Annotation>,
        notes: impl Iterator<Item = Note>,
        patches: impl Iterator<Item = Patch>,
    ) -> Self {
        Self {
            severity,
            span,
            message: message.into(),
            notes: notes.collect(),
            annotations: annotations.collect(),
            patches: patches.collect(),
        }
    }

    /// Severity
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// Associated message
    pub fn message(&self) -> impl Display + '_ {
        self.message.as_str()
    }

    /// Source code span
    pub fn span(&self) -> SourceSpan {
        self.span.dup()
    }

    /// Iterate annotations
    pub fn annotations(&self) -> impl Iterator<Item = Annotation> {
        self.annotations.iter().map(|a| Annotation {
            kind: a.kind.dup(),
            span: a.span.dup(),
            message: a.message.clone(),
        })
    }

    /// Iterate suggested changes
    pub fn patches(&self) -> impl Iterator<Item = Patch> {
        self.patches.iter().map(|p| Patch {
            span: p.span.dup(),
            sub: p.sub.clone(),
            message: p.message.clone(),
        })
    }

    /// Iterate additional notes
    pub fn notes(&self) -> impl Iterator<Item = Note> {
        self.notes.iter().map(|n| Note {
            kind: n.kind.dup(),
            message: n.message.clone(),
        })
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
        assert_eq!(span.start(), Pos::new((1 << 24) - 1, (1 << 20) - 1));
        assert_eq!(span.len(), (1 << 20) - 1);
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
