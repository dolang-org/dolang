use std::fmt::Display;

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

/// Source code position
#[derive(Clone, Debug)]
pub struct Pos {
    offset: usize,
    line: u32,
    column: u32,
}

impl Pos {
    pub(crate) fn new(offset: usize, line: u32, column: u32) -> Self {
        Self {
            offset,
            line,
            column,
        }
    }

    pub(crate) fn dup(&self) -> Self {
        Self {
            offset: self.offset,
            line: self.line,
            column: self.column,
        }
    }

    /// Byte offset
    pub fn byte_offset(&self) -> usize {
        self.offset
    }

    /// Line (starting from 0)
    pub fn line_offset(&self) -> u32 {
        self.line
    }

    /// Column (starting from 0).
    pub fn column_offset(&self) -> u32 {
        self.column
    }

    /// Line number (starting from 1)
    pub fn line_number(&self) -> u32 {
        self.line + 1
    }

    /// Column number (starting from 1)
    pub fn column_number(&self) -> u32 {
        self.column + 1
    }

    fn clone(&self) -> Self {
        Self::new(self.offset, self.line, self.column)
    }
}

/// Source code span
#[derive(Clone, Debug)]
pub struct Span {
    start: Pos,
    end: Pos,
}

impl Span {
    pub(crate) fn dup(&self) -> Self {
        Self {
            start: self.start.dup(),
            end: self.end.dup(),
        }
    }

    pub(crate) fn new(start: Pos, end: Pos) -> Self {
        Self { start, end }
    }

    /// Start of span (inclusive)
    pub fn start(&self) -> Pos {
        self.start.clone()
    }

    /// End of span (exclusive)
    pub fn end(&self) -> Pos {
        self.end.clone()
    }
}

/// Compiler diagnostic
#[derive(Clone)]
pub struct Diag {
    severity: Severity,
    span: Span,
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
    pub(crate) span: Span,
    pub(crate) message: String,
}

impl Annotation {
    /// Kind of annotation
    pub fn kind(&self) -> AnnotationKind {
        self.kind.dup()
    }

    /// Span of annotation in source code
    pub fn span(&self) -> Span {
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
    pub(crate) span: Span,
    pub(crate) message: String,
    pub(crate) sub: String,
}

impl Patch {
    /// Span in source code to change
    pub fn span(&self) -> Span {
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

/// Origin of a resolved identifier
#[derive(Debug)]
pub enum Origin {
    /// Item imported from a module
    ImportItem {
        /// Module span
        module: Span,
        /// Item span
        item: Span,
        /// Name span
        name: Span,
    },
    /// Module import
    ImportModule {
        /// Module span
        module: Span,
        /// Name span
        name: Span,
    },
    /// Module from the prelude
    PreludeModule {
        /// Module name
        module: String,
        /// Binding name
        name: String,
    },
    /// Item from the prelude
    PreludeItem {
        /// Module name
        module: String,
        /// Item name
        item: String,
        /// Bound name
        name: String,
    },
    /// Class definition
    Class {
        /// Definition span
        span: Span,
    },
    /// Function definition
    Def {
        /// Definition span
        span: Span,
    },
    /// Variable binding
    Bind {
        /// Binding span
        span: Span,
    },
    /// Class method definition
    Method {
        /// Definition span
        span: Span,
        /// Enclosing class name span
        class: Span,
    },
    /// Class field declaration
    Field {
        /// Definition span
        span: Span,
        /// Enclosing class name span
        class: Span,
    },
    /// Function parameter
    Param {
        /// Parameter span
        span: Span,
    },
    /// Self parameter (first parameter of a method)
    SelfParam {
        /// Parameter span
        span: Span,
    },
}

impl Diag {
    pub(crate) fn new(
        severity: Severity,
        span: Span,
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
    pub fn span(&self) -> Span {
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
