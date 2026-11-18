use std::fmt::{Debug, Display};

use crate::UnitId;

pub(crate) const CLASS_TOP_LEVEL: &str =
    "`class` and `@class` declarations are only allowed at module top level";

/// Diagnostic severity
///
/// Severities are ordered from least to most severe.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Severity {
    /// Lint
    ///
    /// A likely mistake that may misfire on intended code. Not shown by
    /// default.
    Lint,
    /// Warning
    Warning,
    /// Error
    ///
    /// Always fatal to compilation
    Error,
}

impl Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Lint => write!(f, "lint"),
            Severity::Warning => write!(f, "warning"),
            Severity::Error => write!(f, "error"),
        }
    }
}

/// Which diagnostics a run shows, and which fail it
///
/// A diagnostic at or above the fail threshold fails the run, and is shown
/// even if it is below the show threshold. Errors always fail.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Thresholds {
    show: Severity,
    fail: Severity,
}

impl Default for Thresholds {
    /// Show warnings and errors; fail only on errors.
    fn default() -> Self {
        Self::new(Severity::Warning, Severity::Error)
    }
}

impl Thresholds {
    /// Show diagnostics at or above `show`, and fail on those at or above
    /// `fail`
    pub const fn new(show: Severity, fail: Severity) -> Self {
        Self { show, fail }
    }

    /// Set the least severe diagnostic shown
    pub fn show(&mut self, show: Severity) -> &mut Self {
        self.show = show;
        self
    }

    /// Set the least severe diagnostic that fails
    pub fn fail(&mut self, fail: Severity) -> &mut Self {
        self.fail = fail;
        self
    }

    /// The least severe diagnostic that fails
    pub fn fail_level(&self) -> Severity {
        self.fail
    }

    /// Whether a diagnostic of this severity is shown
    pub fn shows(&self, severity: Severity) -> bool {
        severity >= self.show.min(self.fail)
    }

    /// Whether a diagnostic of this severity fails the run
    pub fn fails(&self, severity: Severity) -> bool {
        severity >= self.fail
    }
}

pub use dolang_bytecode::span::{Pos, Span};

/// No span: where something a typelib omits would be
///
/// Every field is clamped to its maximum, so no real location is mistaken for it.
pub(crate) const INVALID_SPAN: Span = Span::new(u32::MAX, u32::MAX, u32::MAX);

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
    use super::{Severity, Thresholds};

    #[test]
    fn severity_order() {
        assert!(Severity::Lint < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }

    #[test]
    fn default_thresholds() {
        let t = Thresholds::default();
        assert!(!t.shows(Severity::Lint));
        assert!(t.shows(Severity::Warning));
        assert!(t.shows(Severity::Error));
        assert!(!t.fails(Severity::Warning));
        assert!(t.fails(Severity::Error));
    }

    #[test]
    fn failing_diagnostics_are_shown() {
        let t = Thresholds::new(Severity::Error, Severity::Lint);
        assert!(t.shows(Severity::Lint));
        assert!(t.fails(Severity::Lint));
    }

    #[test]
    fn errors_always_fail() {
        let t = Thresholds::new(Severity::Lint, Severity::Error);
        assert!(t.shows(Severity::Lint));
        assert!(!t.fails(Severity::Warning));
        assert!(t.fails(Severity::Error));
    }
}
