use std::{path::Path, sync::Arc};

use super::{Analysis, Entry, Tree, Types};
use crate::{
    Config, Mode, UnitId,
    diag::{Pos, Span},
    typeck::{
        Builder, Role, Style,
        r#type::{Database, Labels},
    },
};

/// A position on the first line
fn at(column: u32) -> Pos {
    Pos::new(0, column)
}

/// An analysis of one unit with a judgment at each span of the first line, by its
/// columns, named by its index
fn analysis(spans: &[(u32, u32)]) -> Analysis {
    let mut db = Database::new();
    db.seal();
    let ty = db.bottom();
    let judgments = spans
        .iter()
        .enumerate()
        .map(|(index, &(start, end))| {
            let entry = Entry {
                role: Role::Read,
                name: Some(index.to_string().into()),
                ty,
                unassigned: false,
                decl: None,
                overload: None,
                item: None,
                group: None,
            };
            (Span::new(0, start, end - start), at(start)..at(end), entry)
        })
        .collect();
    let types = Types {
        db: db.share(),
        labels: Labels::from_iter([]),
    };
    let units = [Some(Tree::new(judgments))].into();
    Analysis::new(Arc::new(types), units, [].into())
}

/// The names of the node at `column` and its ancestors, innermost first
fn chain(analysis: &Analysis, column: u32) -> Vec<String> {
    analysis
        .at(UnitId::from_index(0), at(column))
        .into_iter()
        .flat_map(|node| node.ancestors())
        .flat_map(|node| {
            node.judgments()
                .map(|judged| judged.name().unwrap().to_owned())
        })
        .collect()
}

#[test]
fn nested() {
    // 1: [0, 20) contains 3: [2, 12), which contains 2: [4, 6) and 0: [8, 10)
    let analysis = analysis(&[(8, 10), (0, 20), (4, 6), (2, 12)]);
    assert_eq!(chain(&analysis, 4), ["2", "3", "1"]);
    assert_eq!(chain(&analysis, 9), ["0", "3", "1"]);
    assert_eq!(chain(&analysis, 15), ["1"]);
    let spans: Vec<_> = analysis
        .nodes(UnitId::from_index(0))
        .map(|node| {
            (
                node.span().start().column_offset(),
                node.end().column_offset(),
            )
        })
        .collect();
    assert_eq!(spans, [(0, 20), (2, 12), (4, 6), (8, 10)]);
}

#[test]
fn between_siblings() {
    let analysis = analysis(&[(0, 20), (4, 6), (8, 10)]);
    assert_eq!(chain(&analysis, 6), ["0"]);
    assert_eq!(chain(&analysis, 7), ["0"]);
    // Each sibling's end is past it
    assert_eq!(chain(&analysis, 10), ["0"]);
}

#[test]
fn no_node() {
    let analysis = analysis(&[(4, 6), (8, 10)]);
    assert!(chain(&analysis, 0).is_empty());
    assert!(chain(&analysis, 7).is_empty());
    assert!(chain(&analysis, 30).is_empty());
    assert!(analysis.at(UnitId::from_index(1), at(4)).is_none());
}

#[test]
fn shared_span() {
    let analysis = analysis(&[(0, 20), (4, 6), (4, 6)]);
    assert_eq!(chain(&analysis, 5), ["1", "2", "0"]);
    assert_eq!(analysis.nodes(UnitId::from_index(0)).count(), 2);
}

#[test]
fn enclosing() {
    let analysis = analysis(&[(0, 20), (2, 12), (4, 6), (8, 10)]);
    let name = |start, end| {
        let node = analysis.enclosing(UnitId::from_index(0), at(start)..at(end))?;
        let judged = node.judgments().next().unwrap();
        Some(judged.name().unwrap().to_owned())
    };
    assert_eq!(name(4, 6).as_deref(), Some("2"));
    assert_eq!(name(5, 9).as_deref(), Some("1"));
    assert_eq!(name(5, 14).as_deref(), Some("0"));
    assert_eq!(name(5, 25), None);
}

#[test]
fn checked() {
    let source = "let x = 1\nlet y = x\nlet z = 2\n";
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed);
    let mut builder = Builder::new();
    builder.unit(&unit).unwrap();
    assert!(builder.check().analysis().is_none());

    let mut builder = Builder::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.check().into_analysis().unwrap();
    let judged = |offset: usize, role| {
        let node = analysis
            .at(id, unit.locate(offset..offset).start())
            .unwrap();
        let judged = node.judgments().find(|judged| judged.role() == role);
        let judged = judged.unwrap();
        (judged.name().map(str::to_owned), judged.ty(Style::Reader))
    };
    let read = source.find("= x").unwrap() + 2;
    assert_eq!(
        judged(4, Role::Bind),
        (Some("x".to_owned()), "1".to_owned())
    );
    assert_eq!(
        judged(read, Role::Read),
        (Some("x".to_owned()), "1".to_owned())
    );
    // An expression other than a variable has a value
    let literal = source.find('2').unwrap();
    assert_eq!(judged(literal, Role::Value), (None, "2".to_owned()));
}

#[test]
fn resolved() {
    let source = "\
class A
  pub def get self
    self
class B
@def pick x @ A -> A
@def pick x @ B -> B
def pick x
  x
def run a @ A b @ B
  a.get()
  pick(b)
";
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed);
    let mut builder = Builder::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.check().into_analysis().unwrap();
    let judged = |at: &str, role| {
        let offset = source.find(at).unwrap();
        let node = analysis
            .at(id, unit.locate(offset..offset).start())
            .unwrap();
        node.judgments()
            .find(|judged| judged.role() == role)
            .unwrap()
    };
    // A method call reaches the member its class declares. The receiver and
    // the callee are nodes of their own.
    let member = judged("get()", Role::Member);
    assert_eq!(member.name(), Some("get"));
    let declaration = member.declaration().unwrap();
    assert_eq!((declaration.module(), declaration.name()), ("test", "A"));
    assert_eq!(judged("get()", Role::Callee).overload(), None);
    // A call of an overloaded function chooses by source order
    let callee = judged("(b)", Role::Callee);
    assert_eq!(callee.overload(), Some(1));
    assert!(callee.declaration().is_none());
}

#[test]
fn items() {
    let module = "pub def f x\n  x\n";
    let script = "import a: m\nlet y = m.f(1)\n";
    let mut config = Config::new();
    config
        .typecheck(true)
        .mode(Mode::Module { name: "a".into() });
    let module = config.unit(Path::new("a.dol"), module);
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), script);
    assert!(!module.failed && !unit.failed);
    let mut builder = Builder::new();
    builder.analysis(true);
    builder.unit(&module).unwrap();
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.check().into_analysis().unwrap();
    // A path through a module names its item by the module's own name
    let offset = script.find("m.f").unwrap();
    let node = analysis
        .at(id, unit.locate(offset..offset).start())
        .unwrap();
    let item = node.judgments().find(|judged| judged.role() == Role::Item);
    let declaration = item.unwrap().declaration().unwrap();
    assert_eq!((declaration.module(), declaration.name()), ("a", "f"));
}

#[test]
fn after_non_ascii() {
    // A read after a character of two bytes and a tab on its line
    let source = "let s = 1\nlet t = (\"é\t\", s)\n";
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed);
    let mut builder = Builder::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.check().into_analysis().unwrap();
    let node = analysis.at(id, Pos::new(1, 15)).unwrap();
    assert!(
        node.judgments()
            .any(|judged| judged.role() == Role::Read && judged.name() == Some("s"))
    );
    assert_eq!(unit.text(node.span()), "s");
    assert_eq!(node.end(), Pos::new(1, 16));
}
