//! Tracing the checker under the `typeck` topics of `DOLANG_DEBUG`: `typeck.elab`,
//! `typeck.flow`, `typeck.solver` and `typeck.wf`.

use dolang_util::{debug_enabled, debug_eprintln};

use super::{
    elab::{Tables, Unresolved},
    r#type::{Database, UnitId},
};
use crate::source::Span;

impl Tables<'_> {
    /// A span of `unit` as `path:line:column`
    pub(crate) fn locate(&self, unit: UnitId, span: Span) -> String {
        let info = &self.units[unit.index()];
        if span == Span::INVALID {
            return format!("{}:?", info.path.display());
        }
        let start = super::report::resolve_span(unit, info, span).span().start();
        format!(
            "{}:{}:{}",
            info.path.display(),
            start.line_number(),
            start.column_number()
        )
    }
}

/// Trace what elaboration concluded about each unit's spans
pub(super) fn elab(db: &Database, tables: &Tables<'_>, unresolved: &[Unresolved]) {
    if !debug_enabled!("typeck.elab") {
        return;
    }
    for index in 0..tables.units.len() {
        let unit = UnitId::from_index(index);
        // `typeck.wf` traces undecided checks
        let judgments = tables.judgments(db, unit, unresolved);
        for (name, span, value) in judgments.into_iter().filter(|&(name, ..)| name != "wf") {
            debug_eprintln!(topic: "typeck.elab", "{}: {name}: {value}", tables.locate(unit, span));
        }
    }
}
