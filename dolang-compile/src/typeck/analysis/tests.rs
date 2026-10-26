use std::{path::Path, sync::Arc};

use super::{Analysis, Entry, Tree, Types};
use crate::{
    Config, UnitId,
    source::Span,
    typeck::{
        Builder, Role, Style,
        r#type::{Database, Labels},
    },
};

fn span(start: u32, end: u32) -> Span {
    Span { start, end }
}

/// An analysis of one unit with a judgment at each span, named by its index
fn analysis(spans: &[(u32, u32)]) -> Analysis {
    let mut db = Database::new();
    db.seal();
    let ty = db.bottom();
    let judgments = (spans.iter().enumerate())
        .map(|(index, &(start, end))| {
            let entry = Entry {
                role: Role::Read,
                name: Some(index.to_string().into()),
                ty,
                unassigned: false,
            };
            (span(start, end), entry)
        })
        .collect();
    let types = Types {
        db: db.share(),
        labels: Labels::from_iter([]),
    };
    Analysis::new(Arc::new(types), [Some(Tree::new(judgments))].into())
}

/// The names of the node at `offset` and its ancestors, innermost first
fn chain(analysis: &Analysis, offset: u32) -> Vec<String> {
    (analysis.at(UnitId::from_index(0), offset).into_iter())
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
    let spans: Vec<_> = (analysis.nodes(UnitId::from_index(0)))
        .map(|node| (node.span().start, node.span().end))
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
    assert!(analysis.at(UnitId::from_index(1), 4).is_none());
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
        let node = analysis.enclosing(UnitId::from_index(0), span(start, end))?;
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
    let source = "let x = 1\nlet y = x\n";
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
        let node = analysis.at(id, offset as u32).unwrap();
        let judged = node.judgments().find(|judged| judged.role() == role);
        let judged = judged.unwrap();
        (judged.name().map(str::to_owned), judged.ty(Style::Reader))
    };
    let read = source.rfind('x').unwrap();
    assert_eq!(
        judged(4, Role::Bind),
        (Some("x".to_owned()), "1".to_owned())
    );
    assert_eq!(
        judged(read, Role::Read),
        (Some("x".to_owned()), "1".to_owned())
    );
}
