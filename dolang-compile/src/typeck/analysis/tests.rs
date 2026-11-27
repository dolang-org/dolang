use std::{fmt::Write, path::Path, sync::Arc};

use super::{Analysis, Declaration, Entry, Tree, Types};
use crate::{
    Config, Mode, UnitId,
    diag::{Pos, Span},
    typeck::{
        Check, Role, Style,
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn between_siblings() {
    let analysis = analysis(&[(0, 20), (4, 6), (8, 10)]);
    assert_eq!(chain(&analysis, 6), ["0"]);
    assert_eq!(chain(&analysis, 7), ["0"]);
    // Each sibling's end is past it
    assert_eq!(chain(&analysis, 10), ["0"]);
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn no_node() {
    let analysis = analysis(&[(4, 6), (8, 10)]);
    assert!(chain(&analysis, 0).is_empty());
    assert!(chain(&analysis, 7).is_empty());
    assert!(chain(&analysis, 30).is_empty());
    assert!(analysis.at(UnitId::from_index(1), at(4)).is_none());
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn shared_span() {
    let analysis = analysis(&[(0, 20), (4, 6), (4, 6)]);
    assert_eq!(chain(&analysis, 5), ["1", "2", "0"]);
    assert_eq!(analysis.nodes(UnitId::from_index(0)).count(), 2);
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn checked() {
    let source = "let x = 1\nlet y = x\nlet z = 2\n";
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed);
    let mut builder = Check::new();
    builder.unit(&unit).unwrap();
    assert!(builder.run().analysis().is_none());

    let mut builder = Check::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.run().into_analysis().unwrap();
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
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
    let mut builder = Check::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.run().into_analysis().unwrap();
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
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
    let mut builder = Check::new();
    builder.analysis(true);
    builder.unit(&module).unwrap();
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.run().into_analysis().unwrap();
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
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn after_non_ascii() {
    // A read after a character of two bytes and a tab on its line
    let source = "let s = 1\nlet t = (\"é\t\", s)\n";
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed);
    let mut builder = Check::new();
    builder.analysis(true);
    let id = builder.unit(&unit).unwrap();
    let analysis = builder.run().into_analysis().unwrap();
    let node = analysis.at(id, Pos::new(1, 15)).unwrap();
    assert!(
        node.judgments()
            .any(|judged| judged.role() == Role::Read && judged.name() == Some("s"))
    );
    assert_eq!(unit.text(node.span()), "s");
    assert_eq!(node.end(), Pos::new(1, 16));
}

/// A fixture's script around a rendered type: its declarations, then each row's
/// binding, annotated by `annotate` with the row's index
fn fixture(rows: &[(&str, &str)], annotate: impl Fn(usize) -> String) -> String {
    let mut source = String::from(
        "\
import std
import a
class A
  pub field x = 0
def first[T] items @ std.Array[T] -> T
  items[0]
@def over x @ std.Int -> std.Int
@def over x @ std.Str -> std.Str
def over x
  x
",
    );
    for (index, (name, expr)) in rows.iter().enumerate() {
        let annotation = annotate(index);
        let _ = writeln!(source, "let {name}{annotation} = {expr}");
    }
    // A rigid is seen only in its declaration
    let annotation = annotate(rows.len());
    let _ = writeln!(
        source,
        "def generic[T] x @ T\n  let rigid{annotation} = x\n  rigid"
    );
    source
}

/// The judged types of the bindings named `names`, as `Style::Reader` renders
/// them and as an annotation writes them, and the check's and units'
/// diagnostics, by message
fn bindings(source: &str, names: &[&str]) -> (Vec<(String, Option<String>)>, Vec<String>) {
    let mut config = Config::new();
    config
        .typecheck(true)
        .mode(Mode::Module { name: "std".into() });
    let std = config.unit(
        Path::new("std.dol"),
        include_str!("../../../../dolang-shell-modules/stub/std.dol"),
    );
    config.mode(Mode::Module { name: "a".into() });
    let a = config.unit(Path::new("a.dol"), "pub class B\n");
    config.mode(Mode::Script);
    let unit = config.unit(Path::new("test.dol"), source);
    let messages: Vec<_> = unit
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .collect();
    assert!(
        !std.failed && !a.failed && !unit.failed,
        "{messages:?}\n{source}"
    );
    let mut builder = Check::new();
    builder.analysis(true);
    builder.unit(&std).unwrap();
    builder.unit(&a).unwrap();
    let id = builder.unit(&unit).unwrap();
    let checked = builder.run();
    // A fixture's bindings go unused
    let mut diagnostics: Vec<String> = unit
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .filter(|message| message != "unused variable")
        .chain(
            (checked.diagnostics())
                .filter(|diag| diag.span().unit() == Some(id))
                .map(|diag| diag.message().to_string()),
        )
        .collect();
    diagnostics.sort();
    let analysis = checked.analysis().unwrap();
    // The script's own items bare, and a module's through its import
    let spell = |declaration: Declaration<'_>| {
        let (module, name) = (declaration.module(), declaration.name());
        match module {
            "test" => (!name.contains('.')).then(|| name.to_owned()),
            _ => Some(format!("{module}.{name}")),
        }
    };
    let types = names
        .iter()
        .map(|&name| {
            let judged = analysis
                .nodes(id)
                .flat_map(|node| node.judgments())
                .find(|judged| judged.role() == Role::Bind && judged.name() == Some(name))
                .unwrap_or_else(|| panic!("no binding of {name}"));
            (judged.ty(Style::Reader), judged.written(&spell))
        })
        .collect();
    (types, diagnostics)
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn written_types_parse_as_annotations() {
    // Each binding's value, and the annotation its type is written as
    let rows: &[(&str, &str, Option<&str>)] = &[
        ("int", "1", Some("1")),
        ("negative", "(-1)", Some("-1")),
        ("string", r#""a\$b\"\n""#, Some(r#""a\$b\"\n""#)),
        ("symbol", ":red:", Some(":red:")),
        ("boolean", "true", Some("true")),
        ("none", "nil", Some("nil")),
        ("tuple", r#"(1, "x")"#, Some(r#"(1, "x")"#)),
        ("single", "(1,)", Some("(1,)")),
        ("empty", "()", Some("()")),
        ("pack", "(k: 1, 2)", Some("(k: 1, 2)")),
        ("array", "[1, 2]", Some("std.Array[std.Int]")),
        ("dict", "{a: 1}", Some("std.Dict[std.Sym, std.Int]")),
        ("union", "(nil @ (std.Int | nil))", Some("(nil | std.Int)")),
        (
            "callbacks",
            "(nil @ (((std.Int) -> std.Int) | nil))",
            Some("(((std.Int) -> std.Int) | nil)"),
        ),
        (
            "function",
            "(nil !@ ((std.Int, ?std.Str, *std.Int, k: std.Int) -> std.Int))",
            Some("((std.Int, ?std.Str, *std.Int, k: std.Int) -> std.Int)"),
        ),
        (
            "channels",
            "(nil !@ ((<std.Str, >std.Int) -> nil))",
            Some("((<std.Str, >std.Int) -> nil)"),
        ),
        // A name no declaration refers to is spelled as one would be
        (
            "builtins",
            "(nil !@ std.Dict[std.Value, std.Unknown])",
            Some("std.Dict[std.Value, std.Unknown]"),
        ),
        ("poly", "first", Some("(@[T1] (std.Array[T1]) -> T1)")),
        ("own", "A()", Some("A")),
        ("imported", "a.B()", Some("a.B")),
        // A class object is exact, which no annotation writes
        ("object", "A", None),
        ("generic_class", "std.Array", None),
        ("overloaded", "over", None),
    ];
    let mut names: Vec<_> = rows.iter().map(|&(name, ..)| name).collect();
    names.push("rigid");
    let plain = fixture(
        &rows
            .iter()
            .map(|&(name, expr, _)| (name, expr))
            .collect::<Vec<_>>(),
        |_| String::new(),
    );
    let (types, diagnostics) = bindings(&plain, &names);
    assert_eq!(diagnostics, Vec::<String>::new(), "{plain}");
    let written: Vec<_> = types
        .iter()
        .map(|(_, written)| written.as_deref())
        .collect();
    let mut expected: Vec<_> = rows.iter().map(|&(.., written)| written).collect();
    expected.push(Some("T"));
    assert_eq!(written, expected);

    // Annotated with what it's written as, each binding judges the same type,
    // as does a parameter annotated tightly
    let annotated = fixture(
        &rows
            .iter()
            .map(|&(name, expr, _)| (name, expr))
            .collect::<Vec<_>>(),
        |index| match &types[index].1 {
            Some(written) => format!(" @ {written}"),
            None => String::new(),
        },
    );
    let mut tight = annotated.clone();
    for (index, (name, _, _)) in rows.iter().enumerate() {
        if let Some(written) = &types[index].1 {
            let _ = writeln!(tight, "def tight_{name} a_{name}@{written} b\n  a_{name}");
        }
    }
    let tight_names: Vec<String> = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| types[*index].1.is_some())
        .map(|(_, (name, ..))| format!("a_{name}"))
        .collect();
    let mut all: Vec<&str> = names.clone();
    all.extend(tight_names.iter().map(String::as_str));
    let (again, diagnostics) = bindings(&tight, &all);
    assert_eq!(diagnostics, Vec::<String>::new(), "{tight}");
    // A binding annotated `nil` holds its class, which has no other value
    let same = |judged: &str, original: &str| {
        judged == original || (original, judged) == ("nil", "std.Nil")
    };
    for (index, name) in names.iter().enumerate() {
        let (judged, original) = (&again[index].0, &types[index].0);
        assert!(same(judged, original), "{name}: {judged} for {original}");
    }
    let mut params = again[names.len()..].iter();
    for (index, (name, ..)) in rows.iter().enumerate() {
        if types[index].1.is_some() {
            let (param, _) = params.next().unwrap();
            let original = &types[index].0;
            assert!(same(param, original), "{name}: {param} for {original}");
        }
    }
}
