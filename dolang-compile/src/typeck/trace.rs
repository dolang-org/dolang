//! Tracing the checker under the `typeck` topics of `DOLANG_DEBUG`: `typeck.elab`,
//! `typeck.flow`, `typeck.solver` and `typeck.wf`.

use dolang_util::{debug_enabled, debug_eprintln};

use super::{
    Analysis, Role, Style,
    elab::{Tables, UnitInfo},
    r#type::UnitId,
};
use crate::{diag, source};

impl Tables<'_> {
    /// A span of `unit` as `path:line:column`
    pub(crate) fn locate(&self, unit: UnitId, span: impl Locate) -> String {
        let info = self.info(unit);
        let Some(span) = span.locate(info) else {
            return format!("{}:?", info.path.display());
        };
        let start = span.start();
        format!(
            "{}:{}:{}",
            info.path.display(),
            start.line_number(),
            start.column_number()
        )
    }
}

/// A span traces locate: the surface's, or an offset into the source where it is
/// sliced
pub(crate) trait Locate {
    /// Where it is in a unit, if anywhere
    fn locate(self, info: &UnitInfo<'_>) -> Option<diag::Span>;
}

impl Locate for diag::Span {
    fn locate(self, _: &UnitInfo<'_>) -> Option<diag::Span> {
        (self != diag::Span::INVALID).then_some(self)
    }
}

impl Locate for source::Span {
    fn locate(self, info: &UnitInfo<'_>) -> Option<diag::Span> {
        let source = info.source.filter(|_| self != source::Span::INVALID)?;
        Some(source.file.locate(self))
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
