//! Checks the checker could not decide, reported where they leave code unverified.
//!
//! A strict unit is meant to be checked without passing through `Unknown`, so each
//! check it leaves undecided is a warning: the checker can't verify that code. A
//! residual that shouldn't arise at all (see [`ResidualClass::Internal`]) is an error
//! in any unit, and one an error elsewhere explains (see [`ResidualClass::Cascade`])
//! is never reported. A typelib's declarations were reported when its source was
//! checked, so they are not reported again.

use std::fmt::{self, Write};

use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    elab::{UnitInfo, Unresolved},
    report::{Diag, Report, UnitDiag},
    solver::{Residual, ResidualClass},
    r#type::UnitSpan,
};
use crate::{
    diag::{NoteKind, Severity},
    source::Span,
};

/// The checks left undecided at a span, by why
struct Undecided {
    span: Span,
    residuals: Vec<Residual>,
}

impl Undecided {
    fn internal(&self) -> bool {
        self.residuals
            .iter()
            .any(|residual| residual.class() == ResidualClass::Internal)
    }
}

impl Report for Undecided {
    fn severity(&self) -> Severity {
        match self.internal() {
            true => Severity::Error,
            false => Severity::Warning,
        }
    }

    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, w: &mut dyn Write) -> fmt::Result {
        match self.internal() {
            true => write!(
                w,
                "the checker couldn't decide this, which shouldn't happen"
            ),
            false => write!(w, "the checker can't verify this"),
        }
    }

    fn notes(&self) -> Vec<(NoteKind, String)> {
        if self.internal() {
            let note = "this may come from an inconsistent or stale typelib (rebuild, or \
                        clear the build directory); otherwise it is a checker bug";
            return vec![(NoteKind::Info, note.to_owned())];
        }
        self.residuals
            .iter()
            .map(|residual| {
                let note = match residual {
                    Residual::Unsupported(what) => format!("it doesn't handle {what}"),
                    Residual::Limit => {
                        "it's too complex to check; an annotation may help".to_owned()
                    }
                    Residual::Inference => "nothing determines a type here; annotate it".to_owned(),
                    Residual::GenericArguments => "it can't match type arguments past an \
                                                   expansion; put positional arguments first"
                        .to_owned(),
                    Residual::Alignment => "it can't match up these positional items".to_owned(),
                    Residual::Ambiguous => {
                        "several alternatives remain possible; an annotation may pick one"
                            .to_owned()
                    }
                    Residual::Cycle => {
                        "it can't infer a type that refers to itself; annotate it".to_owned()
                    }
                    Residual::Recursive | Residual::Escape | Residual::Malformed => {
                        unreachable!("an internal residual has its own note")
                    }
                    Residual::MissingIntrinsic(_) => unreachable!("a cascade isn't reported"),
                };
                (NoteKind::Info, note)
            })
            .collect()
    }
}

/// Diagnose the checks in `unresolved` that a reader should see, one per span,
/// skipping a span that `diags` already reports something at.
pub(crate) fn report(units: &[UnitInfo<'_>], unresolved: &[Unresolved], diags: &mut Vec<UnitDiag>) {
    let reported: FxHashSet<UnitSpan> = diags
        .iter()
        .map(|(unit, diag)| UnitSpan {
            unit: *unit,
            span: diag.span(),
        })
        .collect();
    let mut groups: Vec<(UnitSpan, Vec<Residual>)> = Vec::new();
    let mut index: FxHashMap<UnitSpan, usize> = FxHashMap::default();
    for &Unresolved { span, residual } in unresolved {
        let info = &units[span.unit.index()];
        if info.source.is_none()
            || span.span == Span::INVALID
            || reported.contains(&span)
            || residual.class() == ResidualClass::Cascade
        {
            continue;
        }
        let at = *index.entry(span).or_insert_with(|| {
            groups.push((span, Vec::new()));
            groups.len() - 1
        });
        let residuals = &mut groups[at].1;
        if !residuals.contains(&residual) {
            residuals.push(residual);
        }
    }
    for (span, residuals) in groups {
        let undecided = Undecided {
            span: span.span,
            residuals,
        };
        if units[span.unit.index()].strict || undecided.internal() {
            diags.push((span.unit, Diag::new(undecided)));
        }
    }
}
