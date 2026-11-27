//! The checked types of a unit's unannotated bindings, as inlay hints that
//! insert as annotations.
use std::collections::HashMap;

use dolang_compile::{
    Kind, Node, NodeId, Unit,
    diag::Span,
    typeck::{Judged, Role, Style},
};

use crate::{Checked, Spelling};

/// The most characters a hint's label shows of its type before it's cut short
const MAX_LABEL: usize = 40;

/// The checked type of a binding with no annotation, shown after its name
#[derive(Debug, PartialEq, Eq)]
pub struct Hint {
    /// The bound name, which the hint follows
    pub name: Span,
    /// What it shows: ` @ T` for a lone binding, `@T` where bindings share a
    /// line, with its type cut short past [`MAX_LABEL`] characters
    pub label: String,
    /// Its type in full, when the label cuts it short
    pub tooltip: Option<String>,
    /// The text inserting it as an annotation, if source can write its type
    pub edit: Option<String>,
}

/// The bindings a hint can annotate, and the bindings each shares its
/// syntax with
struct Site {
    name: Span,
    /// The bindings it shares a syntax site with, which settle its spacing
    group: (NodeId, Option<Span>),
    annotated: bool,
}

/// The hints of `unit`'s bindings that have no annotation, in source order:
/// `let`, `bind`, `if let`, `while let` and `for` names, and `do` block
/// parameters. A binding gets none when its type says nothing of a value
/// (see [`Judged::vague`]) or repeats a literal it's bound to.
///
/// A hint shows the type where the name is bound, not one narrowed after.
pub fn hints<R>(unit: &Unit<'_>, checked: &Checked<'_, R>) -> Vec<Hint> {
    let mut judged = HashMap::new();
    for node in checked.analysis.nodes(checked.unit) {
        let span = node.span();
        let bind = node.judgments().find(|judged| judged.role() == Role::Bind);
        if let Some(bind) = bind {
            judged.entry((span.start(), span.len())).or_insert(bind);
        }
    }
    let sites: Vec<_> = unit
        .nodes()
        .filter_map(|(_, node)| site(unit, node))
        .collect();
    // Where each group's bindings start their lines
    let mut lines: HashMap<_, Vec<u32>> = HashMap::new();
    for site in &sites {
        let line = site.name.start().line_offset();
        lines.entry(site.group).or_default().push(line);
    }
    let spelling = Spelling::new(unit, checked.name);
    let mut hints: Vec<_> = sites
        .iter()
        .filter(|site| !site.annotated)
        .filter_map(|site| {
            let judged = *judged.get(&(site.name.start(), site.name.len()))?;
            if judged.vague() || judged.literal() {
                return None;
            }
            let line = site.name.start().line_offset();
            let shared = lines[&site.group].iter().filter(|&&at| at == line).count() > 1;
            Some(hint(site.name, judged, &spelling, shared))
        })
        .collect();
    hints.sort_by_key(|hint| hint.name);
    hints
}

/// A binding that can take an annotation, if `node` is one
fn site(unit: &Unit<'_>, node: Node<'_>) -> Option<Site> {
    let parent = node.parent()?;
    let parent_kind = unit.node(parent)?.kind();
    let (name, group) = match node.kind() {
        Kind::Bind { name, .. } => {
            let span = node.span()?;
            if span.start() != name.start() {
                // A `let` or `bind` statement, which all its names share
                (name, Some(span))
            } else if matches!(
                parent_kind,
                Kind::For | Kind::ForElem | Kind::If | Kind::While
            ) {
                (name, None)
            } else {
                return None;
            }
        }
        Kind::PositionalParam { name, .. } | Kind::KeyParam { name, .. }
            if matches!(parent_kind, Kind::Lambda) =>
        {
            (name, None)
        }
        _ => return None,
    };
    let annotated = node.children().any(|child| {
        let kind = unit.node(child).map(|child| child.kind());
        matches!(kind, Some(Kind::Type { .. }))
    });
    Some(Site {
        name,
        group: (parent, group),
        annotated,
    })
}

fn hint(name: Span, judged: Judged<'_>, spelling: &Spelling<'_>, shared: bool) -> Hint {
    let sep = if shared { "@" } else { " @ " };
    let written = spelling.written(judged);
    let ty = (written.clone()).unwrap_or_else(|| judged.annotation(Style::Reader));
    let edit = written.map(|written| format!("{sep}{written}"));
    let (label, tooltip) = match ty.char_indices().nth(MAX_LABEL - 1) {
        Some((cut, _)) if ty.chars().count() > MAX_LABEL => {
            (format!("{sep}{}…", &ty[..cut]), Some(ty))
        }
        _ => (format!("{sep}{ty}"), None),
    };
    Hint {
        name,
        label,
        tooltip,
        edit,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::{Config, Mode, typeck::Check};

    use super::*;

    /// The std stub's unit, and a script's, compiled for a check
    fn units(source: &str) -> (Unit<'static>, Unit<'static>) {
        let mut config = Config::new();
        config
            .typecheck(true)
            .document(true)
            .mode(Mode::Module { name: "std".into() });
        let std = config.unit(
            Path::new("std.dol"),
            include_str!("../../dolang-shell-modules/stub/std.dol"),
        );
        config.mode(Mode::Script);
        let unit = config.unit(Path::new("test.dol"), source.to_owned());
        let messages: Vec<_> = (std.diagnostics().chain(unit.diagnostics()))
            .map(|diag| diag.message().to_string())
            .filter(|message| message != "unused variable")
            .collect();
        assert_eq!(messages, [] as [String; 0], "{source}");
        (std, unit)
    }

    /// A hint where its name ends, by line and column from 1
    type Found = (u32, u32, String, Option<String>, Option<String>);

    /// Each hint of a script, as `(line, column, label, edit, tooltip)`, and
    /// the check's diagnostics of the script
    fn checked(source: &str) -> (Vec<Found>, Vec<String>) {
        let (std, unit) = units(source);
        let mut builder = Check::new();
        builder.analysis(true);
        builder.unit(&std).unwrap();
        let id = builder.unit(&unit).unwrap();
        let result = builder.run();
        let diagnostics = (result.diagnostics())
            .filter(|diag| diag.span().unit() == Some(id))
            .map(|diag| diag.message().to_string())
            .filter(|message| message != "unused variable")
            .collect();
        let checked = Checked {
            analysis: result.analysis().unwrap(),
            unit: id,
            name: "test",
            resolve: (),
        };
        let found = (hints(&unit, &checked).into_iter())
            .map(|hint| {
                let end = unit.end(hint.name);
                let (line, column) = (end.line_number(), end.column_number());
                (line, column, hint.label, hint.edit, hint.tooltip)
            })
            .collect();
        (found, diagnostics)
    }

    /// A hint as `checked` finds it, with an edit of its label
    fn hint(line: u32, column: u32, label: &str) -> Found {
        (line, column, label.to_owned(), Some(label.to_owned()), None)
    }

    #[test]
    fn unannotated_bindings_are_hinted() {
        const LONG: &str = "Array[Dict[Sym, Array[Float] | Array[Int] | Int | Sym | std.Nil]]";
        let source = "\
let a = (1 + 2)
let b c = [1, 2]
let d @ Int = (1 + 2)
let e f@Int = [1, 2]
for k v = {a: 1, b: 2}
  (k, v)
def h p
  p
let lit = 1
if let m = [1].get(0)
  m
bind [1, 2]
  - x
  - y
@def over x @ Int -> Int
@def over x @ Str -> Str
def over x
  x
let o = over
let long = [{a: 1, b: [(1 + 2)], c: :x:, d: nil, e: [(1 + 2.0)]}]
";
        let (hints, diagnostics) = checked(source);
        assert_eq!(diagnostics, [] as [String; 0]);
        assert_eq!(
            hints,
            [
                hint(1, 6, " @ Int"),
                hint(2, 6, "@Int"),
                hint(2, 8, "@Int"),
                hint(4, 6, "@Int"),
                hint(5, 6, "@(:a: | :b:)"),
                hint(5, 8, "@(1 | 2)"),
                hint(10, 9, " @ Int"),
                hint(13, 6, " @ Int"),
                hint(14, 6, " @ Int"),
                // An overloaded function can't be written
                (19, 6, " @ test.over".to_owned(), None, None),
                // A long type is cut short, and shown whole in the tooltip
                (
                    20,
                    9,
                    " @ Array[Dict[Sym, Array[Float] | Array[In…".to_owned(),
                    Some(format!(" @ {LONG}")),
                    Some(LONG.to_owned()),
                ),
            ]
        );
    }

    #[test]
    fn strict_blocks() {
        let source = "\
# dolang: strict
let f @ ((Int) -> Int) = (do |x| x + 1)
let g @ ((Int, Int) -> Int) = (do |x y| x + y)
let h @ ((Int) -> Int) = do |x|
  x
";
        let (hints, diagnostics) = checked(source);
        assert_eq!(diagnostics, [] as [String; 0]);
        assert_eq!(
            hints,
            [
                hint(2, 32, " @ Int"),
                hint(3, 37, "@Int"),
                hint(3, 39, "@Int"),
                hint(4, 31, " @ Int"),
            ]
        );
    }

    /// A source with each of its hints' edits inserted, and the hints with none
    fn edited(source: &str) -> (String, Vec<Found>) {
        let (hints, _) = checked(source);
        let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
        let mut kept = vec![];
        for (line, column, label, edit, tooltip) in hints.into_iter().rev() {
            let Some(edit) = edit else {
                kept.push((line, column, label, None, tooltip));
                continue;
            };
            let text = &mut lines[line as usize - 1];
            let at =
                (text.char_indices().nth(column as usize - 1)).map_or(text.len(), |(at, _)| at);
            text.insert_str(at, &edit);
        }
        kept.reverse();
        (lines.join("\n") + "\n", kept)
    }

    /// Inserted, every edit is an annotation that checks, and leaves its
    /// binding no hint
    #[test]
    fn edits_check() {
        for source in [
            "let a = (1 + 2)\nlet b c = [1, 2]\nfor k v = {a: 1, b: 2}\n  (k, v)\n",
            "if let m = [1].get(0)\n  m\nbind [1, 2]\n  - x\n  - y\n",
            "let long = [{a: 1, b: [(1 + 2)], c: :x:, d: nil, e: [(1 + 2.0)]}]\n",
            "# dolang: strict\nlet g @ ((Int, Int) -> Int) = (do |x y| x + y)\n",
            "def over x\n  x\nlet o = [over]\n",
        ] {
            let (edited, kept) = edited(source);
            let (hints, diagnostics) = checked(&edited);
            assert_eq!(diagnostics, [] as [String; 0], "{edited}");
            assert_eq!(hints, kept, "{edited}");
        }
    }
}
