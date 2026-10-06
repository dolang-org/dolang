//! What the reporting pass diagnoses.

use std::fmt::{self, Write};

use crate::{
    diag::{NoteKind, Severity},
    source::Span,
    typeck::report::Report,
};

/// A diagnosed problem, with the types it names rendered
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// A numbered hole after a gap stays an integer key.
    FmtGap {
        span: Span,
        index: i128,
        missing: usize,
    },
    /// An argument that doesn't fit its parameter, whose type is shown when it's
    /// known, with notes for each deeper part of it that doesn't fit: what
    /// doesn't fit, then each type found around it
    Argument {
        span: Span,
        found: String,
        expected: Option<String>,
        causes: Vec<Vec<String>>,
    },
    /// A call that doesn't pass one of its callee's required parameters
    MissingArgument(Span),
    /// An argument the callee has no parameter for
    ExtraArgument(Span),
    /// A call that doesn't fit its callee in some other way
    Call { span: Span, callee: String },
    /// A call none of its callee's overloads accepts, with a note for each
    /// overload saying why
    NoOverload {
        span: Span,
        callee: Option<String>,
        notes: Vec<String>,
    },
    /// A call several of its callee's overloads accept, with nothing dynamic in
    /// what it passes to excuse it
    AmbiguousCall {
        span: Span,
        callee: Option<String>,
        survivors: Vec<String>,
    },
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
    /// A value that doesn't fit the type it's cast to
    Cast {
        span: Span,
        found: String,
        ty: String,
    },
    /// An unchecked cast whose value can be shown to fit its type
    Assertion { span: Span, ty: String },
    /// A default that neither fits its variable's annotation nor is a sentinel
    Default {
        span: Span,
        found: String,
        annotation: String,
    },
    /// A member the receiver doesn't have, or alternatives of the union `within`
    /// don't
    MissingMember {
        span: Span,
        receivers: Vec<String>,
        within: Option<String>,
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
            Problem::FmtGap { span, .. }
            | Problem::Argument { span, .. }
            | Problem::MissingArgument(span)
            | Problem::ExtraArgument(span)
            | Problem::Call { span, .. }
            | Problem::NoOverload { span, .. }
            | Problem::AmbiguousCall { span, .. }
            | Problem::Conflict(span)
            | Problem::Unadmitted { span, .. }
            | Problem::Misfit { span, .. }
            | Problem::Impossible { span, .. }
            | Problem::Annotation { span, .. }
            | Problem::Cast { span, .. }
            | Problem::Assertion { span, .. }
            | Problem::Default { span, .. }
            | Problem::MissingMember { span, .. }
            | Problem::MemberUse { span, .. }
            | Problem::Unassigned { span, .. } => span,
        }
    }

    fn severity(&self) -> Severity {
        match self {
            Self::FmtGap { .. } | Self::Assertion { .. } => Severity::Warning,
            _ => Severity::Error,
        }
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        match self {
            Problem::Argument { causes, .. } => (causes.iter().flatten())
                .map(|note| (NoteKind::Info, note.clone()))
                .collect(),
            Problem::NoOverload { notes, .. } => (notes.iter())
                .map(|note| (NoteKind::Info, note.clone()))
                .collect(),
            Problem::AmbiguousCall { survivors, .. } => (survivors.iter())
                .map(|survivor| (NoteKind::Info, format!("`{survivor}` accepts them")))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self {
            Problem::FmtGap { index, missing, .. } => write!(
                w,
                "format hole `#{index}` follows missing `#{missing}`; it is an integer key, not a positional item"
            ),
            // Where the argument's type and the parameter's look alike, only the
            // causes show what doesn't fit
            Problem::Argument {
                found,
                expected: Some(expected),
                ..
            } if found != expected => write!(w, "expected `{expected}`, found `{found}`"),
            Problem::Argument { found, .. } => write!(w, "`{found}` does not fit this parameter"),
            Problem::MissingArgument(_) => write!(w, "this call is missing an argument"),
            Problem::ExtraArgument(_) => write!(w, "the callee takes no such argument"),
            Problem::Call { callee, .. } => write!(w, "this call does not fit `{callee}`"),
            Problem::NoOverload {
                callee: Some(callee),
                ..
            } => write!(w, "no overload of `{callee}` accepts these arguments"),
            Problem::NoOverload { callee: None, .. } => {
                write!(w, "no overload accepts these arguments")
            }
            Problem::AmbiguousCall {
                callee: Some(callee),
                ..
            } => write!(w, "several overloads of `{callee}` accept these arguments"),
            Problem::AmbiguousCall { callee: None, .. } => {
                write!(w, "several overloads accept these arguments")
            }
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
            Problem::Cast { found, ty, .. } => {
                write!(w, "`{found}` does not fit the cast's type `{ty}`")
            }
            Problem::Assertion { ty, .. } => {
                write!(w, "this value fits `{ty}`, so `@` suffices")
            }
            Problem::Default {
                found, annotation, ..
            } => write!(
                w,
                "default `{found}` does not fit the annotation `{annotation}`, and isn't a `nil` or symbol sentinel"
            ),
            Problem::MissingMember {
                receivers,
                within,
                name,
                ..
            } => {
                let quoted: Vec<_> = receivers.iter().map(|r| format!("`{r}`")).collect();
                let receivers = match &quoted[..] {
                    [.., last] if quoted.len() > 1 => {
                        format!("{} and {last}", quoted[..quoted.len() - 1].join(", "))
                    }
                    _ => quoted.concat(),
                };
                let has = if quoted.len() > 1 { "have" } else { "has" };
                match within {
                    Some(within) => write!(w, "{receivers} in `{within}` {has} no member `{name}`"),
                    None => write!(w, "{receivers} {has} no member `{name}`"),
                }
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
