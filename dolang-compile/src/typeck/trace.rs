//! Tracing the checker under the `typeck` topics of `DOLANG_DEBUG`: `typeck.elab`,
//! `typeck.flow`, `typeck.solver` and `typeck.wf`.

use dolang_util::{debug_enabled, debug_eprintln};

use super::{Analysis, Role, Style, elab::Tables, r#type::UnitId};
use crate::source::Span;

impl Tables<'_> {
    /// A span of `unit` as `path:line:column`
    pub(crate) fn locate(&self, unit: UnitId, span: Span) -> String {
        let info = self.info(unit);
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

/// Trace what elaboration concluded about each unit's spans, as the analysis
/// has it
pub(super) fn elab(analysis: &Analysis, tables: &Tables<'_>) {
    if !debug_enabled!("typeck.elab") {
        return;
    }
    for index in 0..tables.shards.len() {
        let unit = UnitId::from_index(index);
        for node in analysis.nodes(unit) {
            for judged in node.judgments() {
                let role = judged.role();
                if !matches!(role, Role::Reference | Role::Annotation | Role::Declaration) {
                    continue;
                }
                let ty = judged.ty(Style::Full);
                let location = tables.locate(unit, node.span());
                debug_eprintln!(topic: "typeck.elab", "{location}: {role:?}: {ty}");
            }
        }
    }
}
