use std::{
    cell::OnceCell,
    fmt::{self, Debug, Write},
    ops::{BitOr, Range},
    path::{Path, PathBuf},
};

use crate::diag::{self, Annotation, AnnotationKind, NoteKind, Pos, Severity};

use dolang_util::arena::ArenaVec;

use super::Compiler;

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
    content: &'s [u8],
    newlines: OnceCell<Vec<Offset>>,
}

impl<'s> File<'s> {
    pub(crate) fn new(path: &Path, content: &'s [u8]) -> Self {
        File {
            path: path.to_owned(),
            content,
            newlines: Default::default(),
        }
    }

    pub(crate) fn content(&self) -> &'s [u8] {
        self.content
    }

    pub(crate) fn slice(&self, span: Span) -> &'s [u8] {
        &self.content[span.start as usize..span.end as usize]
    }

    pub(crate) fn str(&self, span: Span) -> &'s str {
        str::from_utf8(self.slice(span)).expect("invalid utf-8")
    }

    fn newlines(&self) -> &[Offset] {
        self.newlines.get_or_init(|| {
            let mut newlines: Vec<Offset> = Default::default();
            let mut iter = self.content.iter();
            let mut cur = 0usize;

            while let Some(pos) = iter.position(|&c| c == b'\n') {
                newlines.push(Offset::try_from(cur + pos).unwrap());
                cur += pos + 1;
            }

            newlines
        })
    }

    pub(crate) fn coord(&self, offset: Offset) -> Coord {
        let newlines = self.newlines();
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

    pub(crate) fn coord_span(&self, span: Span) -> Range<Coord> {
        self.coord(span.start)..self.coord(span.end)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) trait Annotate {
    fn kind(&self) -> AnnotationKind;
    fn span(&self) -> Span;
    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Note {
    fn kind(&self) -> NoteKind;
    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Patch {
    fn span(&self) -> Span;
    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result;
    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result;
}

pub(crate) trait Diagnose {
    fn span(&self) -> Span;
    fn severity(&self) -> Severity;
    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result;

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

pub struct Diag(Box<dyn Diagnose>);

impl Diag {
    pub(crate) fn new(info: impl Diagnose + 'static) -> Self {
        Self(Box::new(info))
    }

    fn resolve_span(compiler: &Compiler<'_>, span: Span) -> diag::Span {
        let coords = compiler.file.coord_span(span);
        diag::Span::new(
            Pos::new(span.start as usize, coords.start.line, coords.start.column),
            Pos::new(span.end as usize, coords.end.line, coords.end.column),
        )
    }

    pub(crate) fn resolve<'a>(&self, compiler: &Compiler<'a>) -> diag::Diag {
        let span = Self::resolve_span(compiler, self.0.span());
        let mut msg = String::new();
        self.0.message(compiler, &mut msg).unwrap();
        diag::Diag::new(
            self.0.severity(),
            span,
            msg,
            self.0.annotations().map(|a| {
                let span = Self::resolve_span(compiler, a.span());
                let mut message = String::new();
                a.message(compiler, &mut message).unwrap();
                Annotation {
                    kind: a.kind(),
                    span,
                    message,
                }
            }),
            self.0.notes().map(|n| {
                let mut message = String::new();
                n.message(compiler, &mut message).unwrap();
                diag::Note {
                    kind: n.kind(),
                    message,
                }
            }),
            self.0.patches().map(|p| {
                let span = Self::resolve_span(compiler, p.span());
                let mut sub = String::new();
                p.sub(compiler, &mut sub).unwrap();
                let mut message = String::new();
                p.message(compiler, &mut message).unwrap();
                diag::Patch { span, sub, message }
            }),
        )
    }
}

pub(crate) struct Diags {
    vec: ArenaVec<Diag>,
}

impl Diags {
    pub(crate) fn new() -> Self {
        Self {
            vec: ArenaVec::new(),
        }
    }

    pub(crate) fn push(&self, info: impl Diagnose + 'static) {
        self.vec.push(Diag::new(info))
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = Diag> {
        self.vec.drain()
    }
}
