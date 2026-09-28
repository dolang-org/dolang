//! What the reporting pass diagnoses.

use std::fmt::{self, Write};

use crate::{
    Compiler,
    diag::Severity,
    source::{Diagnose, Span},
};

/// A diagnosed problem, with the types it names rendered
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// An argument that doesn't fit its parameter, whose type is shown when it's
    /// known
    Argument {
        span: Span,
        found: String,
        expected: Option<String>,
    },
    /// A call that doesn't pass one of its callee's required parameters
    MissingArgument(Span),
    /// An argument the callee has no parameter for
    ExtraArgument(Span),
    /// A call that doesn't fit its callee in some other way
    Call { span: Span, callee: String },
    /// A value that doesn't fit what its use requires of it
    Misfit {
        span: Span,
        found: String,
        misfit: Misfit,
    },
    /// A value that doesn't fit a variable's annotation, or a function's declared
    /// result
    Annotation {
        span: Span,
        found: String,
        annotation: String,
        result: bool,
    },
    /// A variable read where it may not be assigned yet
    Unassigned {
        span: Span,
        name: String,
        definitely: bool,
    },
}

/// What a value is required to be
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Misfit {
    /// A part of a binary string
    Binary,
    /// A format specification's width or precision
    Int,
    Iterable,
    Spreadable,
    Unpackable,
}

impl Diagnose for Problem {
    fn span(&self) -> Span {
        match *self {
            Problem::Argument { span, .. }
            | Problem::MissingArgument(span)
            | Problem::ExtraArgument(span)
            | Problem::Call { span, .. }
            | Problem::Misfit { span, .. }
            | Problem::Annotation { span, .. }
            | Problem::Unassigned { span, .. } => span,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self {
            Problem::Argument {
                found,
                expected: Some(expected),
                ..
            } => write!(w, "expected `{expected}`, found `{found}`"),
            Problem::Argument {
                found,
                expected: None,
                ..
            } => write!(w, "`{found}` does not fit this parameter"),
            Problem::MissingArgument(_) => write!(w, "this call is missing an argument"),
            Problem::ExtraArgument(_) => write!(w, "the callee takes no such argument"),
            Problem::Call { callee, .. } => write!(w, "this call does not fit `{callee}`"),
            Problem::Misfit { found, misfit, .. } => {
                let required = match misfit {
                    Misfit::Binary => "binary",
                    Misfit::Int => "an `Int`",
                    Misfit::Iterable => "iterable",
                    Misfit::Spreadable => "spreadable",
                    Misfit::Unpackable => "unpackable",
                };
                write!(w, "`{found}` is not {required}")
            }
            Problem::Annotation {
                found,
                annotation,
                result: false,
                ..
            } => write!(w, "`{found}` does not fit the annotation `{annotation}`"),
            Problem::Annotation {
                found,
                annotation,
                result: true,
                ..
            } => write!(
                w,
                "`{found}` does not fit the declared result `{annotation}`"
            ),
            Problem::Unassigned {
                name,
                definitely: true,
                ..
            } => write!(w, "`{name}` is read before it is assigned"),
            Problem::Unassigned {
                name,
                definitely: false,
                ..
            } => write!(w, "`{name}` may be read before it is assigned"),
        }
    }
}
