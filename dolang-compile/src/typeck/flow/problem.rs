//! What the reporting pass diagnoses.

use std::fmt::{self, Write};

use crate::{diag::Severity, source::Span, typeck::report::Report};

/// A diagnosed problem, with the types it names rendered
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// An argument that doesn't fit its parameter, whose type is shown when it's
    /// known, with the part of it that doesn't fit what, when that's deeper
    Argument {
        span: Span,
        found: String,
        expected: Option<String>,
        inner: Option<(String, String)>,
    },
    /// A call that doesn't pass one of its callee's required parameters
    MissingArgument(Span),
    /// An argument the callee has no parameter for
    ExtraArgument(Span),
    /// A call that doesn't fit its callee in some other way
    Call { span: Span, callee: String },
    /// A call whose types index a schema with a key that may be one of its
    /// positions' indexes
    Conflict(Span),
    /// A call whose types select by a key its schema doesn't admit
    Unadmitted { span: Span, key: String },
    /// A value that doesn't fit what its use requires of it
    Misfit {
        span: Span,
        found: String,
        misfit: Misfit,
    },
    /// A pattern that can't unpack a value of the type found
    Impossible { span: Span, found: String },
    /// A value that doesn't fit a variable's annotation, or a function's declared
    /// result
    Annotation {
        span: Span,
        found: String,
        annotation: String,
        result: bool,
    },
    /// A default that neither fits its variable's annotation nor is a sentinel
    Default {
        span: Span,
        found: String,
        annotation: String,
    },
    /// A member the receiver doesn't have
    MissingMember {
        span: Span,
        receiver: String,
        name: String,
    },
    /// A member used in a way its kind doesn't allow
    MemberUse {
        span: Span,
        name: String,
        misuse: MemberUse,
    },
    /// A variable read where it may not be assigned yet
    Unassigned {
        span: Span,
        name: String,
        definitely: bool,
    },
}

/// How a member is misused
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberUse {
    /// Reading a property without a getter
    Read,
    /// Writing a property without a setter
    Write,
    /// Assigning to a method
    Method,
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

impl Report for Problem {
    fn span(&self) -> Span {
        match *self {
            Problem::Argument { span, .. }
            | Problem::MissingArgument(span)
            | Problem::ExtraArgument(span)
            | Problem::Call { span, .. }
            | Problem::Conflict(span)
            | Problem::Unadmitted { span, .. }
            | Problem::Misfit { span, .. }
            | Problem::Impossible { span, .. }
            | Problem::Annotation { span, .. }
            | Problem::Default { span, .. }
            | Problem::MissingMember { span, .. }
            | Problem::MemberUse { span, .. }
            | Problem::Unassigned { span, .. } => span,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self {
            // Where the argument's type and the parameter's look alike, only what's
            // inside them shows what doesn't fit
            Problem::Argument {
                found,
                expected: Some(expected),
                inner: Some((part, bound)),
                ..
            } if found == expected => {
                write!(w, "`{part}` does not fit `{bound}` in `{found}`")
            }
            Problem::Argument {
                found,
                expected: Some(expected),
                inner,
                ..
            } => {
                write!(w, "expected `{expected}`, found `{found}`")?;
                match inner {
                    Some((part, bound)) => write!(w, ": `{part}` does not fit `{bound}`"),
                    None => Ok(()),
                }
            }
            Problem::Argument {
                found,
                expected: None,
                inner,
                ..
            } => {
                write!(w, "`{found}` does not fit this parameter")?;
                match inner {
                    Some((part, bound)) => write!(w, ": `{part}` does not fit `{bound}`"),
                    None => Ok(()),
                }
            }
            Problem::MissingArgument(_) => write!(w, "this call is missing an argument"),
            Problem::ExtraArgument(_) => write!(w, "the callee takes no such argument"),
            Problem::Call { callee, .. } => write!(w, "this call does not fit `{callee}`"),
            Problem::Conflict(_) => write!(
                w,
                "this call indexes a schema with a key that may be one of its positions' indexes"
            ),
            Problem::Unadmitted { key, .. } => {
                write!(
                    w,
                    "this call selects by `{key}`, which isn't one of the schema's keys"
                )
            }
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
            Problem::Impossible { found, .. } => {
                write!(w, "`{found}` can never unpack as this pattern")
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
            Problem::Default {
                found, annotation, ..
            } => write!(
                w,
                "default `{found}` does not fit the annotation `{annotation}`, and isn't a `nil` or symbol sentinel"
            ),
            Problem::MissingMember { receiver, name, .. } => {
                write!(w, "`{receiver}` has no member `{name}`")
            }
            Problem::MemberUse { name, misuse, .. } => match misuse {
                MemberUse::Read => write!(w, "`{name}` has no getter"),
                MemberUse::Write => write!(w, "`{name}` has no setter"),
                MemberUse::Method => write!(w, "`{name}` is a method, which can't be assigned"),
            },
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
