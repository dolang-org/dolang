//! The checker's diagnostics, which locate spans by line and column, so a unit
//! checked without its source is reported like any other.

use std::fmt::{self, Write};

use super::r#type::UnitId;
use crate::diag::{self, AnnotationKind, NoteKind, Severity, SourceSpan, Span};

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

pub(crate) struct Diag(Box<dyn Report + Send>);

impl Diag {
    pub(crate) fn new(info: impl Report + Send + 'static) -> Self {
        Self(Box::new(info))
    }

    pub(crate) fn span(&self) -> Span {
        self.0.span()
    }

    /// The report as a diagnostic of `unit`
    pub(crate) fn resolve(&self, unit: UnitId) -> diag::Diag {
        let mut message = String::new();
        let _ = self.0.message(&mut message);
        diag::Diag::new(
            self.0.severity(),
            resolve_span(unit, self.0.span()),
            message,
            self.0
                .annotations()
                .into_iter()
                .map(|annotation| diag::Annotation {
                    kind: annotation.kind,
                    span: resolve_span(unit, annotation.span),
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

/// A span of `unit`, as a diagnostic's location
pub(crate) fn resolve_span(unit: UnitId, span: Span) -> SourceSpan {
    // A span a typelib omits is at the start of its first line
    let span = match span == Span::INVALID {
        true => Span::new(0, 0, 0),
        false => span,
    };
    SourceSpan::new(Some(unit), span)
}
