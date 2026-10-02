//! The checker's diagnostics, which resolve to locations through a unit's line
//! table, so a unit checked without its source is reported like any other.

use std::fmt::{self, Write};

use super::{elab::UnitInfo, r#type::UnitId};
use crate::{
    diag::{self, AnnotationKind, NoteKind, Pos, Severity, SourceSpan},
    source::{self, Span},
};

/// A problem the checker reports
pub(crate) trait Report {
    fn severity(&self) -> Severity;
    fn span(&self) -> Span;
    fn message(&self, w: &mut dyn Write) -> fmt::Result;

    /// Other spans the problem points to
    fn annotations(&self) -> Vec<Annotation> {
        Vec::new()
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        Vec::new()
    }
}

/// A span a report points to besides its own, in the same unit
pub(crate) struct Annotation {
    pub(crate) kind: AnnotationKind,
    pub(crate) span: Span,
    pub(crate) message: String,
}

/// A report, with the unit whose spans it points into
pub(crate) type UnitDiag = (UnitId, Diag);

pub(crate) struct Diag(Box<dyn Report>);

impl Diag {
    pub(crate) fn new(info: impl Report + 'static) -> Self {
        Self(Box::new(info))
    }

    pub(crate) fn span(&self) -> Span {
        self.0.span()
    }

    /// Resolve the report's locations in `unit`.
    pub(crate) fn resolve(&self, unit: UnitId, info: &UnitInfo<'_>) -> diag::Diag {
        let mut message = String::new();
        let _ = self.0.message(&mut message);
        diag::Diag::new(
            self.0.severity(),
            resolve_span(unit, info, self.0.span()),
            message,
            self.0
                .annotations()
                .into_iter()
                .map(|annotation| diag::Annotation {
                    kind: annotation.kind,
                    span: resolve_span(unit, info, annotation.span),
                    message: annotation.message,
                }),
            self.0
                .notes()
                .into_iter()
                .map(|(kind, message)| diag::Note { kind, message }),
            std::iter::empty(),
        )
    }
}

/// A span of `unit`, resolved to lines and columns
pub(crate) fn resolve_span(unit: UnitId, info: &UnitInfo<'_>, span: Span) -> SourceSpan {
    let pos = |offset| {
        let coord = source::coord(&info.newlines, offset);
        Pos::new(offset as usize, coord.line, coord.column)
    };
    SourceSpan::new(Some(unit), diag::Span::new(pos(span.start), pos(span.end)))
}
