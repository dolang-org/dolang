use std::{
    borrow::Cow,
    fmt::{self, Debug, Write},
    ops::{BitOr, Range},
    path::{Path, PathBuf},
};

use crate::diag::{self, Annotation, AnnotationKind, NoteKind, Pos, Severity};

use dolang_util::{frozen::Frozen, mono::MonoVec};

use super::UnitId;

pub(crate) type Offset = u32;

#[derive(Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct Span {
    pub start: Offset,
    pub end: Offset,
}

impl Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

impl Span {
    pub(crate) const INVALID: Span = Span {
        start: Offset::MAX,
        end: Offset::MIN,
    };

    pub(crate) fn left_char(&self) -> Self {
        if self.start == self.end {
            Self {
                start: self.start,
                end: self.end,
            }
        } else {
            Self {
                start: self.start,
                end: self.start + 1,
            }
        }
    }

    pub(crate) fn right_char(&self) -> Self {
        if self.start == self.end {
            Self {
                start: self.start,
                end: self.end,
            }
        } else {
            Self {
                start: self.end - 1,
                end: self.end,
            }
        }
    }

    pub(crate) fn after_right_char(&self) -> Self {
        Self {
            start: self.end,
            end: self.end + 1,
        }
    }

    pub(crate) fn before_left_char(&self) -> Self {
        Self {
            start: self.start - 1,
            end: self.start,
        }
    }
}

impl BitOr<Span> for &Span {
    type Output = Span;

    fn bitor(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl BitOr<&Span> for &Span {
    type Output = Span;

    fn bitor(self, other: &Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl BitOr<&Span> for Span {
    type Output = Span;

    fn bitor(self, other: &Self) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl BitOr<Span> for Span {
    type Output = Span;

    fn bitor(self, other: Self) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl<T: Into<Offset>> From<Range<T>> for Span {
    fn from(value: Range<T>) -> Self {
        Span {
            start: value.start.into(),
            end: value.end.into(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Coord {
    pub line: Offset,
    pub column: Offset,
}

impl From<(Offset, Offset)> for Coord {
    fn from((line, column): (Offset, Offset)) -> Self {
        Coord { line, column }
    }
}

#[derive(Clone)]
pub(crate) struct File<'s> {
    path: PathBuf,
    content: Cow<'s, str>,
    newlines: Vec<Offset>,
}

impl<'s> File<'s> {
    pub(crate) fn new(path: &Path, content: Cow<'s, str>) -> Self {
        let mut newlines = Vec::new();
        let mut iter = content.bytes();
        let mut cur = 0usize;
        while let Some(pos) = iter.position(|c| c == b'\n') {
            newlines.push(Offset::try_from(cur + pos).unwrap());
            cur += pos + 1;
        }
        File {
            path: path.to_owned(),
            content,
            newlines,
        }
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    pub(crate) fn slice(&self, span: Span) -> &[u8] {
        &self.content.as_bytes()[span.start as usize..span.end as usize]
    }

    /// The text of `span`, whose ends the lexer put on character boundaries
    pub(crate) fn str(&self, span: Span) -> &str {
        &self.content[span.start as usize..span.end as usize]
    }

    /// The offset of each newline in the file
    pub(crate) fn newlines(&self) -> &[Offset] {
        &self.newlines
    }

    pub(crate) fn coord(&self, offset: Offset) -> Coord {
        coord(self.newlines(), offset)
    }

    pub(crate) fn coord_span(&self, span: Span) -> Range<Coord> {
        self.coord(span.start)..self.coord(span.end)
    }

    /// Where a line starts
    fn line_start(&self, line: u32) -> usize {
        match line.checked_sub(1) {
            Some(prior) => self
                .newlines
                .get(prior as usize)
                .map_or(self.content.len(), |&newline| newline as usize + 1),
            None => 0,
        }
    }

    /// The location of `span`, in characters
    pub(crate) fn locate(&self, span: Span) -> diag::Span {
        let coord = self.coord(span.start);
        let line = span.start - coord.column;
        diag::Span::new(
            coord.line,
            chars(self.slice(Span::from(line..span.start))),
            chars(self.slice(span)),
        )
    }

    /// Where `span` lies in the content, in bytes. A clamped span stops short.
    pub(crate) fn byte_range(&self, span: diag::Span) -> Range<usize> {
        let start = span.start();
        let line = self.line_start(start.line_offset());
        let advance = |from: usize, count: u32| {
            (self.content[from..].char_indices())
                .nth(count as usize)
                .map_or(self.content.len(), |(offset, _)| from + offset)
        };
        let begin = advance(line, start.column_offset());
        begin..advance(begin, span.len())
    }

    /// Where `span` ends
    pub(crate) fn end(&self, span: diag::Span) -> Pos {
        let end = self.byte_range(span).end as Offset;
        let coord = self.coord(end);
        Pos::new(
            coord.line,
            chars(self.slice(Span::from(end - coord.column..end))),
        )
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// The number of characters in `text`, which may cut one
pub(crate) fn chars(text: &[u8]) -> u32 {
    let count = text.iter().filter(|&&byte| byte & 0xC0 != 0x80).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The line and column of an offset into a file whose newlines are at `newlines`
pub(crate) fn coord(newlines: &[Offset], offset: Offset) -> Coord {
    let index = newlines.partition_point(|&o| offset > o);
    if index == 0 {
        Coord {
            line: 0,
            column: offset,
        }
    } else {
        Coord {
            line: index as u32,
            column: offset - newlines[index - 1] - 1,
        }
    }
}

pub(crate) trait Annotate {
    fn kind(&self) -> AnnotationKind;
    fn span(&self) -> Span;
    fn message(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Note {
    fn kind(&self) -> NoteKind;
    fn message(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Patch {
    fn span(&self) -> Span;
    fn message(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result;
    fn sub(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Diagnose {
    fn span(&self) -> Span;
    fn severity(&self) -> Severity;
    fn message(&self, file: &File<'_>, w: &mut dyn Write) -> fmt::Result;

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        Box::new([].into_iter())
    }

    fn notes(&self) -> Box<dyn Iterator<Item = Box<dyn Note>>> {
        Box::new([].into_iter())
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([].into_iter())
    }
}

pub struct Diag(Box<dyn Diagnose + Send + Sync>);

impl Diag {
    pub(crate) fn new(info: impl Diagnose + Send + Sync + 'static) -> Self {
        Self(Box::new(info))
    }

    pub(crate) fn resolve(&self, file: &File<'_>) -> diag::Diag {
        self.resolve_in(file, None)
    }

    /// Resolve the diagnostic, naming `unit` as the one its locations are in.
    pub(crate) fn resolve_in(&self, file: &File<'_>, unit: Option<UnitId>) -> diag::Diag {
        let span = diag::SourceSpan::new(unit, file.locate(self.0.span()));
        let mut msg = String::new();
        self.0.message(file, &mut msg).unwrap();
        diag::Diag::new(
            self.0.severity(),
            span,
            msg,
            self.0.annotations().map(|a| {
                let span = diag::SourceSpan::new(unit, file.locate(a.span()));
                let mut message = String::new();
                a.message(file, &mut message).unwrap();
                Annotation {
                    kind: a.kind(),
                    span,
                    message,
                }
            }),
            self.0.notes().map(|n| {
                let mut message = String::new();
                n.message(file, &mut message).unwrap();
                diag::Note {
                    kind: n.kind(),
                    message,
                }
            }),
            self.0.patches().map(|p| {
                let span = diag::SourceSpan::new(unit, file.locate(p.span()));
                let mut sub = String::new();
                p.sub(file, &mut sub).unwrap();
                let mut message = String::new();
                p.message(file, &mut message).unwrap();
                diag::Patch { span, sub, message }
            }),
        )
    }
}

pub(crate) struct Diags {
    vec: MonoVec<Diag>,
}

impl Diags {
    pub(crate) fn new() -> Self {
        Self {
            vec: MonoVec::new(),
        }
    }

    pub(crate) fn push(&self, info: impl Diagnose + Send + Sync + 'static) {
        self.vec.push(Diag::new(info))
    }

    pub(crate) fn freeze(self) -> Frozen<MonoVec<Diag>> {
        Frozen::new(self.vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "é\tx = ü\r\nnext ab\n\nend";

    fn file() -> File<'static> {
        File::new(Path::new("test.dol"), TEXT.into())
    }

    /// The location of the first `text` in [`TEXT`]
    fn find(file: &File<'_>, text: &str) -> diag::Span {
        let start = TEXT.find(text).unwrap();
        file.locate(Span::from(start as u32..(start + text.len()) as u32))
    }

    #[test]
    fn locates_in_characters() {
        let file = file();
        let x = find(&file, "x");
        assert_eq!((x.start().line_offset(), x.start().column_offset()), (0, 2));
        let u = find(&file, "ü");
        assert_eq!((u.start().column_offset(), u.len()), (6, 1));
        let ab = find(&file, "ab");
        assert_eq!(
            (ab.start().line_offset(), ab.start().column_offset()),
            (1, 5)
        );
    }

    #[test]
    fn slices_and_ends() {
        let file = file();
        for text in ["é", "x = ü", "ü\r\nnext", "ab\n\nend", "\n\n", "end", ""] {
            let span = find(&file, text);
            assert_eq!(&TEXT[file.byte_range(span)], text, "{span:?}");
        }
        // Each newline counts, `\r\n` as two
        let across = find(&file, "ü\r\nnext");
        assert_eq!(across.len(), 7);
        let end = file.end(across);
        assert_eq!((end.line_offset(), end.column_offset()), (1, 4));
        let end = file.end(find(&file, "end"));
        assert_eq!((end.line_offset(), end.column_offset()), (3, 3));
    }

    #[test]
    fn clamped_span_stops_short() {
        let file = file();
        let past = diag::Span::new(1, 5, u32::MAX);
        assert_eq!(&TEXT[file.byte_range(past)], "ab\n\nend");
        let beyond = diag::Span::new(9, 0, 1);
        assert_eq!(file.byte_range(beyond), TEXT.len()..TEXT.len());
    }
}
