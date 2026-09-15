use std::{
    borrow::Cow,
    fmt::{self, Write},
};

use crate::{
    Compiler,
    diag::{AnnotationKind, NoteKind, Severity},
    lex::Op,
    source::{Annotate, Diagnose, Note, Patch, Span},
};

pub(super) struct SyntaxDiag {
    pub(super) span: Span,
    pub(super) message: Cow<'static, str>,
}

impl SyntaxDiag {
    pub(super) fn new(span: Span, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl Diagnose for SyntaxDiag {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "{}", self.message)
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.span
    }
}

pub(super) struct InvalidLValue(pub(super) Span);

impl Diagnose for InvalidLValue {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression isn't a valid assignment target")
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.0
    }
}

pub(super) struct InvalidCompactOp(pub(super) Op, pub(super) Span);

impl Diagnose for InvalidCompactOp {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "operator not allowed in compact expressions: {}", self.0)
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.1
    }
}

#[derive(Copy, Clone)]
pub(super) struct ImplicitDelimitedConcat {
    pub(super) span: Span,
    pub(super) insert: Span,
}

impl Patch for ImplicitDelimitedConcat {
    fn span(&self) -> Span {
        Span {
            start: self.insert.start,
            end: self.insert.start,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "insert `$` if you intended for the entire argument to be an expression"
        )
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "$")
    }
}

impl Diagnose for ImplicitDelimitedConcat {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "implicit concatenation not permitted after delimited expression"
        )
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(*self) as Box<dyn Patch>].into_iter())
    }
}

pub(super) struct AmbigIndex(pub(super) Span, pub(super) Span);

enum AmbigIndexPatchKind {
    NoSpace,
    Parens,
}

struct AmbigIndexPatch(AmbigIndexPatchKind, Span, Span);

impl Patch for AmbigIndexPatch {
    fn span(&self) -> Span {
        match self.0 {
            AmbigIndexPatchKind::NoSpace => self.1 | self.2,
            AmbigIndexPatchKind::Parens => self.2,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self.0 {
            AmbigIndexPatchKind::NoSpace => write!(w, "remove space"),
            AmbigIndexPatchKind::Parens => {
                write!(w, "wrap with parentheses to call with a singleton list")
            }
        }
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let index = compiler.file.str(self.2);
        match self.0 {
            AmbigIndexPatchKind::NoSpace => {
                let lhs = compiler.file.str(self.1);
                write!(w, "{lhs}{index}")
            }
            AmbigIndexPatchKind::Parens => write!(w, "({index})"),
        }
    }
}

impl Diagnose for AmbigIndex {
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "index separated by whitespace is misleading")
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn span(&self) -> Span {
        self.0 | self.1
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new(
            [
                Box::new(AmbigIndexPatch(
                    AmbigIndexPatchKind::NoSpace,
                    self.0,
                    self.1,
                )) as Box<dyn Patch>,
                Box::new(AmbigIndexPatch(AmbigIndexPatchKind::Parens, self.0, self.1)),
            ]
            .into_iter(),
        )
    }
}

pub(super) struct BadFloat(pub(super) Span);

impl Diagnose for BadFloat {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "invalid floating point constant: {}",
            compiler.file.str(self.0)
        )
    }

    fn span(&self) -> Span {
        self.0
    }
}

pub(super) struct BadFmtParamName(pub(super) Span);

impl Diagnose for BadFmtParamName {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "parameter position must be a non-negative integer")
    }

    fn span(&self) -> Span {
        self.0
    }
}

pub(super) struct FmtParamOutsideSeq(pub(super) Span);

impl Diagnose for FmtParamOutsideSeq {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "a parameter is only valid in a `t\"...\"` sequence, which keeps \
             its segments apart to fill in later"
        )
    }

    fn span(&self) -> Span {
        self.0
    }
}

#[derive(Copy, Clone)]
pub(super) struct MisleadingArg {
    pub(super) arg0_span: Span,
    pub(super) arg_span: Span,
    pub(super) patch_span: Span,
}

impl Patch for MisleadingArg {
    fn span(&self) -> Span {
        Span {
            start: self.patch_span.start,
            end: self.patch_span.start,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "insert a space")
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, " ")
    }
}

impl Annotate for MisleadingArg {
    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }

    fn span(&self) -> Span {
        self.arg0_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression is actually an argument to this function")
    }
}

impl Diagnose for MisleadingArg {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "juxtaposed argument is misleading")
    }

    fn span(&self) -> Span {
        self.arg_span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(*self) as Box<dyn Patch>].into_iter())
    }

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        Box::new([Box::new(*self) as Box<dyn Annotate>].into_iter())
    }
}

pub(super) struct NonConstExpr(pub(super) Span);

impl Diagnose for NonConstExpr {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "expression is not a constant")
    }
}

pub(super) struct InvalidConstType(pub(super) Span);

impl Diagnose for InvalidConstType {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "a constant type must be a symbol, string, integer, boolean, or `nil`"
        )
    }
}

pub(super) struct OptionalTypeArg(pub(super) Span);

impl Diagnose for OptionalTypeArg {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "type arguments cannot be optional")
    }
}

pub(super) struct OptionalRest(pub(super) Span);

impl Diagnose for OptionalRest {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "a rest item cannot be optional")
    }
}

pub(super) struct ParamsWithoutArrow(pub(super) Span);

impl Diagnose for ParamsWithoutArrow {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "parenthesized type is a parameter list, which must be followed by `->`"
        )
    }
}

pub(super) struct RequiredAfterOptional(pub(super) Span);

impl Diagnose for RequiredAfterOptional {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "required positional items must precede any optional positional items"
        )
    }
}

pub(super) struct RestMustBeTrailing(pub(super) Span);

impl Diagnose for RestMustBeTrailing {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "rest parameter must be trailing")
    }
}

#[derive(Clone)]
pub(super) struct MisleadingDollar(pub(super) Span);

impl Diagnose for MisleadingDollar {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "misleading `$`")
    }

    fn notes(&self) -> Box<dyn Iterator<Item = Box<dyn Note>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Note>].into_iter())
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new(
            [
                Box::new(MisleadingDollarPatch::Remove(self.0)) as Box<dyn Patch>,
                Box::new(MisleadingDollarPatch::Insert(Span {
                    start: self.0.end,
                    end: self.0.end,
                })),
            ]
            .into_iter(),
        )
    }
}

impl Note for MisleadingDollar {
    fn kind(&self) -> NoteKind {
        NoteKind::Info
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "`$` is always a low-precedence call in full expression contexts"
        )
    }
}

enum MisleadingDollarPatch {
    Remove(Span),
    Insert(Span),
}

impl Patch for MisleadingDollarPatch {
    fn span(&self) -> Span {
        match self {
            MisleadingDollarPatch::Remove(span) | MisleadingDollarPatch::Insert(span) => *span,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self {
            MisleadingDollarPatch::Remove(_) => write!(w, "remove the `$`"),
            MisleadingDollarPatch::Insert(_) => write!(w, "insert a space"),
        }
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        match self {
            MisleadingDollarPatch::Remove(_) => Ok(()),
            MisleadingDollarPatch::Insert(_) => write!(w, " "),
        }
    }
}

pub(super) struct SpecialMethodOutsideClass(pub(super) Span);

impl Diagnose for SpecialMethodOutsideClass {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "special methods are only valid in a class body")
    }
}
