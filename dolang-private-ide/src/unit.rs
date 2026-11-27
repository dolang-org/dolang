//! Queries of one unit's declarations and source text.
use dolang_compile::{
    Kind, NodeId, Token, Unit,
    diag::Span,
    typeck::{self, Role},
};

/// A token naming a declaration
#[derive(Clone, Copy, Debug)]
pub struct Reference {
    pub span: Span,
    pub id: NodeId,
}

/// The tokens of `unit` naming a declaration, sorted by start. A prelude
/// binding has no declaration in the source, but names a bundled item.
pub fn references(unit: &Unit<'_>) -> Vec<Reference> {
    let mut refs = Vec::new();
    unit.tokens(&mut |token, span: Span, node: Option<NodeId>, _| {
        let punctuation = matches!(token, Token::Delim | Token::Sigil);
        let Some(id) = node.filter(|_| !span.is_empty() && !punctuation) else {
            return;
        };
        let Some(node) = unit.node(id) else {
            return;
        };
        let named = node.definition().is_some()
            || matches!(
                node.kind(),
                Kind::PreludeModule { .. } | Kind::PreludeItem { .. }
            );
        if named {
            refs.push(Reference { span, id });
        }
    });
    refs.sort_by_key(|reference| reference.span.start());
    refs
}

/// The reference of `refs`, sorted by start, under the start of `at` when
/// it's empty, or else the one spanning exactly `at`
pub fn reference_at<'r>(unit: &Unit<'_>, refs: &'r [Reference], at: Span) -> Option<&'r Reference> {
    let start = at.start();
    let before = refs.partition_point(|reference| reference.span.start() <= start);
    refs.get(before.checked_sub(1)?)
        .filter(|reference| match at.is_empty() {
            true => start < unit.end(reference.span),
            false => reference.span.start() == start && unit.end(reference.span) == unit.end(at),
        })
}

/// The overload chosen by the call of what `node` names. A method call is
/// judged at its member, and a function call at the whole call, which starts
/// where its callee does. A union receiver's call reaches several.
pub fn chosen(node: typeck::Node<'_>) -> Option<u32> {
    let start = node.span().start();
    let callees = node
        .ancestors()
        .take_while(|ancestor| ancestor.span().start() == start)
        .map(|ancestor| {
            let callees = ancestor.judgments();
            callees
                .filter(|judged| judged.role() == Role::Callee)
                .collect::<Vec<_>>()
        })
        .find(|callees| !callees.is_empty())?;
    match callees[..] {
        [callee] => callee.overload(),
        _ => None,
    }
}

/// The `index`th `@def` overload of the function or method `id`, in source
/// order, or `id` itself when it has no such overload
pub fn overload(unit: &Unit<'_>, id: NodeId, index: u32) -> NodeId {
    let overloads = named_overloads(unit, id);
    let chosen = usize::try_from(index)
        .ok()
        .and_then(|index| overloads.get(index));
    chosen.copied().unwrap_or(id)
}

/// The `@def` overloads of the function or method `id`, in source order: none
/// when it has none, or is one itself
pub fn overloads(unit: &Unit<'_>, id: NodeId) -> Vec<NodeId> {
    match is_overload(unit, id) {
        true => Vec::new(),
        false => named_overloads(unit, id),
    }
}

/// The `@def` overloads named as the declaration `id` is, in source order
fn named_overloads(unit: &Unit<'_>, id: NodeId) -> Vec<NodeId> {
    let found = || {
        let node = unit.node(id)?;
        let name = unit.text(node.definition()?);
        let mut overloads: Vec<_> = unit
            .node(node.parent()?)?
            .children()
            .filter(|&other| is_overload(unit, other))
            .filter_map(|other| {
                let node = unit.node(other)?;
                let named = unit.text(node.definition()?) == name;
                named.then_some((node.span()?.start(), other))
            })
            .collect();
        overloads.sort_unstable_by_key(|&(start, _)| start);
        Some(overloads.into_iter().map(|(_, id)| id).collect())
    };
    found().unwrap_or_default()
}

/// Whether a declaration is an `@def` overload signature
pub(crate) fn is_overload(unit: &Unit<'_>, id: NodeId) -> bool {
    let Some(node) = unit.node(id) else {
        return false;
    };
    match node.kind() {
        Kind::Function { type_only, .. } => type_only,
        // A protocol's own member is type-only too, but is written without `@`
        Kind::Method {
            type_only: true, ..
        }
        | Kind::SpecialMethod {
            type_only: true, ..
        } => {
            let (Some(span), Some(name)) = (node.span(), node.definition()) else {
                return false;
            };
            let start = unit.byte_range(span).start;
            let before = unit.source().get(start..unit.byte_range(name).start);
            let def = before.and_then(|before| before.trim_end().strip_suffix("def"));
            def.is_some_and(|def| def.trim_end().ends_with('@'))
        }
        _ => false,
    }
}

/// A doc comment's text without its comment markers, keeping its Markdown
/// layout
pub fn normalize_doc(doc: &str) -> String {
    doc.split('\n')
        .map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('#').unwrap_or(line);
            line.strip_prefix(' ').unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Source text as one line
pub fn one_line(unit: &Unit<'_>, span: Span) -> String {
    unit.text(span)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The source text at `span` with its lines kept, each after the first
/// indented as it would be under the declaration `decl` at the start of a line
pub fn as_written(unit: &Unit<'_>, decl: Span, span: Span) -> String {
    let source = unit.source();
    let start = unit.byte_range(decl).start;
    let line = &source[source[..start].rfind('\n').map_or(0, |at| at + 1)..];
    let indent = &line[..line.len() - line.trim_start().len()];
    unit.text(span)
        .lines()
        .enumerate()
        .map(|(index, text)| match index {
            0 => text.trim(),
            _ => text
                .strip_prefix(indent)
                .unwrap_or(text.trim_start())
                .trim_end(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::Config;

    use super::*;

    #[test]
    fn doc_comments_preserve_markdown_after_removing_markers() {
        assert_eq!(
            normalize_doc("  # Summary.  \n  #\n  #   indented"),
            "Summary.\n\n  indented"
        );
    }

    #[test]
    fn references_are_the_names_of_declarations() {
        // Characters before them are two and four bytes
        let source = "# é 😀\nlet x = 1\nlet y = [x]\n";
        let mut config = Config::new();
        config.document(true);
        let unit = config.unit(Path::new("refs.dol"), source);
        let refs = references(&unit);
        let names: Vec<_> = refs
            .iter()
            .map(|reference| unit.text(reference.span))
            .collect();
        assert_eq!(names, ["x", "y", "x"]);
        // A position in a reference, and a span that is exactly one
        let x = refs[2];
        let at = |span| reference_at(&unit, &refs, span).map(|found| found.span);
        let start = x.span.start();
        let inside = unit.locate(unit.byte_range(x.span).start..unit.byte_range(x.span).start);
        assert_eq!(at(inside), Some(x.span));
        assert_eq!(at(x.span), Some(x.span));
        assert_eq!(start.line_number(), 3);
        // Past its end, and the `[` before it
        let end = unit.byte_range(x.span).end;
        assert_eq!(at(unit.locate(end..end)), None);
        let bracket = unit.byte_range(x.span).start - 1;
        assert_eq!(at(unit.locate(bracket..bracket + 1)), None);
    }
}
