use std::path::Path;

use super::{Layout, Problem, Results, Tree};
use crate::{
    Config,
    source::Span,
    typeck::{
        Builder, Check,
        cfg::{BlockId, FuncKind, Ir, Origin, Terminal},
        solver::Residual,
        r#type::{TypeId, UnitId},
    },
};

/// A unit's flow results with each fact's type rendered, its facts in source
/// order
#[derive(Debug, PartialEq)]
struct Rendered {
    facts: Vec<((u32, u32), String, bool)>,
    problems: Vec<Problem>,
    unresolved: Vec<(Span, Residual)>,
}

/// Each fact of `results` by span, its type rendered by `render`
fn rendered(
    results: &Results,
    render: impl Fn(TypeId) -> String,
) -> impl Iterator<Item = ((u32, u32), String, bool)> {
    (results.facts.iter())
        .map(move |(span, fact)| ((span.start, span.end), render(fact.ty), fact.unassigned))
}

/// Analyze a unit's runs serially, each in queue order or in reverse, gathering
/// their results as a check does
fn analyze(check: &Check<'_>, unit: usize, reversed: bool) -> Rendered {
    let ir = check.cfgs[unit]
        .as_ref()
        .expect("a unit with source is lowered");
    let layout = Layout::new(ir);
    let runs = ir.runs();
    let outputs = Tree {
        ir,
        layout: &layout,
        runs: &runs,
        layer: check.layers[unit]
            .as_ref()
            .expect("a lowered unit has a layer"),
        tables: &check.tables,
        reversed,
        outputs: (0..runs.len()).map(|_| None).collect(),
    }
    .finish();
    let mut facts = Vec::new();
    let mut problems = Vec::new();
    let mut unresolved = Vec::new();
    for (db, results) in &outputs {
        facts.extend(rendered(results, |ty| check.tables.render_type(db, ty)));
        problems.extend(results.problems.iter().cloned());
        unresolved.extend(results.unresolved.iter().copied());
    }
    facts.sort();
    super::sort(&mut problems, &mut unresolved);
    Rendered {
        facts,
        problems,
        unresolved,
    }
}

/// Analyze a unit both in queue order and in reverse, which must agree with
/// each other and with the check
fn agreed(check: &Check<'_>, unit: usize) -> Rendered {
    let forward = analyze(check, unit, false);
    let reversed = analyze(check, unit, true);
    assert_eq!(forward, reversed);
    let flow = check.flows[unit]
        .as_ref()
        .expect("a unit with source is analyzed");
    let mut facts: Vec<_> = rendered(flow, |ty| check.render_fact(ty)).collect();
    facts.sort();
    let checked = Rendered {
        facts,
        problems: flow.problems.clone(),
        unresolved: flow.unresolved.clone(),
    };
    assert_eq!(forward, checked);
    forward
}

/// The rendered types of the facts at spans of `unit` whose text is `name`
fn facts_named(check: &Check<'_>, unit: usize, name: &str) -> Vec<String> {
    let flow = check.flows[unit]
        .as_ref()
        .expect("a unit with source is analyzed");
    (flow.facts.iter())
        .filter(|(span, _)| check.tables.text(UnitId::from_index(unit), **span) == name)
        .map(|(_, fact)| check.render_fact(fact.ty))
        .collect()
}

/// Analyze a script both in queue order and in reverse, which must agree
fn agree(source: &str) {
    agree_with(source, |_| {});
}

fn agree_with(source: &str, inspect: impl FnOnce(&Check<'_>)) {
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    let diags: Vec<_> = unit
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .collect();
    assert!(!unit.failed, "the fixture doesn't compile: {diags:?}");
    let mut builder = Builder::new();
    builder.unit(&unit).unwrap();
    let check = builder.check();
    agreed(&check, 0);
    inspect(&check);
}

#[test]
fn callable_instances_and_unions() {
    agree_with(
        "
class A
class B
class Callable
  pub def (call) _self x @ A -> B
    ...
class Child: Callable
class Generic[T]
  pub def (call)[U] _self x @ T f @ ((T) -> U) -> U
    ...
class Picker
  @def (call) self x @ A -> A
  @def (call) self x @ B -> B
  pub def (call) _self x
    x
class Empty
class Sink[T]
  pub def (call) _self f @ ((T) -> A) -> A
    ...
def run c @ Callable child @ Child g @ Generic[A] p @ Picker a @ A b @ B mixed @ (((A) -> A) | Callable) sinks @ (Sink[A] | Sink[B])
  let instance = c $a
  let inherited = child $a
  let generic = g $a do |x| b
  let overloaded = p $a
  let union = mixed $a
  let callback_union = sinks do |input|
    let _ = input
    a
  let _ = [instance, inherited, generic, overloaded, union, callback_union]
  c $b
def dynamic x e @ Empty
  let unknown = x()
  let _ = unknown
  e()
",
        |check| {
            let flow = check.flows[0].as_ref().unwrap();
            for (name, expected) in [
                ("instance", ".B"),
                ("inherited", ".B"),
                ("generic", ".B"),
                ("overloaded", ".A"),
                ("union", ".A | test.B"),
                ("input", ".A | test.B"),
                ("callback_union", ".A"),
                ("unknown", "Unknown"),
            ] {
                let types = facts_named(check, 0, name);
                assert!(!types.is_empty(), "no facts for {name}");
                assert!(
                    types.iter().all(|ty| ty.ends_with(expected)),
                    "{name}: {types:?}",
                );
            }
            assert_eq!(flow.problems.len(), 2, "{:?}", flow.problems);
            assert!(flow.problems.iter().any(|problem| {
                matches!(problem, super::Problem::Argument { .. })
            }));
            assert!(flow.problems.iter().any(|problem| {
                matches!(problem, super::Problem::MissingMember { name, .. } if name == "(call)")
            }));
            assert!(flow.unresolved.is_empty(), "{:?}", flow.unresolved);
        },
    );
}

/// A run detached late reads what the run it's detached from finished with,
/// including its rigids
#[test]
fn late_runs_inherit() {
    agree_with(
        "
let greeting = :hi:
greeting = :hello:
def show()
  let seen = greeting
  seen
def outer[T] x @ T
  let y = x
  def inner()
    let z = y
    z
  inner
",
        |check| {
            assert_eq!(check.cfgs[0].as_ref().unwrap().runs().len(), 4);
            for (name, expected) in [("seen", ":hello:"), ("z", "test.outer.#0")] {
                let types = facts_named(check, 0, name);
                assert!(!types.is_empty(), "no facts for {name}");
                assert!(types.iter().all(|ty| ty == expected), "{name}: {types:?}");
            }
        },
    );
}

#[test]
fn order_independence() {
    agree(
        "
def f()
  nil
let x = 1
let y = nil
while x
  if y
    y = x
  else
    x = :a:
  for z = [x, y]
    y = z
try
  y = f()
catch e
  x = e
finally
  x = y
let g = do
  x = [x]
g()
",
    );
}

#[test]
fn lambdas_are_order_independent() {
    agree(
        "
def map[T, U] xs @ Array[T] f @ ((T) -> U) -> Array[U]
  ...
def fold[T] init @ T f @ ((T, T) -> T) -> T
  ...
let ys = map [1, 2] do |x| [x]
let zs = map $ys do |y| fold 0 do |a _b| a
let f = do |n @ Int| n
let g = do |n @ Int| f $n
f = do |n @ Int| g $n
g 1
",
    );
}

#[test]
fn rounds_are_order_independent() {
    agree(
        "
class A
class B: A
def id[T] x @ T -> T
  x
def pick a @ A b @ B
  let x = id a
  while x
    x = id b
    let y = pick x b
    for z = [x, y]
      x = id z
  x
",
    );
}

#[test]
fn rebuilt_collections_are_order_independent() {
    agree(
        "
def first[T] xs @ Array[T] -> T
  ...
def rebuilt c @ Bool n @ Int s @ Str
  let x = n
  while c
    let ys = [x]
    let d = {k: x}
    if c
      x = first [s]
    else
      x = first [ys, d]
",
    );
}

#[test]
fn optional_patterns_are_order_independent() {
    agree(
        r#"
class A
class B
let ?k: (a: a = 1 j: (A(b = a) | B(b))) = ()
let capture = do b
let f = do |?key: (p = 1 q = p)| -> nil
  let _ = (p, q)
  nil
if let ?key: (A(n = 1) | B(n)) = ()
  let g = do n
  g()
else
  f()
let result = match (())
  ?key: (p = false q = (p || 1)) do (p, q)
for ?key: (p = 1) = [(), ()]
  f key: (p,)
"#,
    );
}

#[test]
fn captures_keep_outer_binders_before_inner_binders() {
    agree_with(
        "
def outer[T, U @ Array[T]] _u @ U
  def middle[V @ U] _v @ V
    @let Capture = (V, U)
    let result @ Capture = nil
    result
  middle
",
        |check| {
            let (index, _) = check
                .tables
                .decls
                .iter()
                .enumerate()
                .find(|(_, decl)| {
                    decl.name
                        .is_some_and(|name| check.tables.text(decl.unit, name.span) == "Capture")
                })
                .unwrap();
            let lifted = &check.tables.lifted[&crate::typeck::r#type::DeclId::from_index(index)];
            // T is captured through U's bound, even though the alias names V first.
            assert_eq!(lifted.len(), 3);
            assert_eq!(
                lifted.iter().map(|binder| binder.slot).collect::<Vec<_>>(),
                [0, 1, 0]
            );
            assert_eq!(lifted[0].decl, lifted[1].decl);
            assert!(lifted[1].decl < lifted[2].decl);
        },
    );
}

#[test]
fn templates_are_order_independent() {
    let mut config = Config::new();
    config
        .typecheck(true)
        .mode(crate::Mode::Module { name: "std".into() });
    let std = config.unit(
        Path::new("std.dol"),
        include_str!("../../../../dolang/stub/std.dol"),
    );
    assert!(!std.failed);
    let source = r#"
import std:
  - @Fmt
pub def keep[S @ {*, *(Int | Sym): Value}] f @ Fmt[Int, S] -> Fmt[Int, S]
  f
@def choose f @ Fmt[Int, {a: Int}] -> Int
@def choose f @ Fmt[Int, {b: Str}] -> Str
pub def choose f
  f
pub def run n @ Int width @ Int text @ Str
  let seed = t"${#earlier_hole} ${#later_hole}"
  let ordered = t"${#later_hole} ${#1} ${#earlier_hole} ${#0} ${#later_hole}"
  let _ = (seed, ordered)
  let generic = keep t"${n:$width} ${#a}"
  generic.format a: 1
  let chosen = choose t"$n ${#a}"
  let _ = chosen
  let nested = t"$generic ${#outer}"
  nested.format outer: true
  let direct @ Fmt[Int, {a: Str}] = keep t"$n ${#a}"
  direct.format a: 1
  let gap = t"${#0} ${#2} ${#2}"
  let _ = gap
  let bad_width = t"${n:$text}"
  let _ = bad_width
"#;
    config.mode(crate::Mode::Script);
    let script = config.unit(Path::new("test.dol"), source);
    assert!(!script.failed);
    let mut builder = Builder::new();
    builder.unit(&std).unwrap();
    builder.unit(&script).unwrap();
    let check = builder.check();
    let forward = agreed(&check, 1);
    assert!(forward.unresolved.is_empty(), "{:?}", forward.unresolved);
    assert_eq!(forward.problems.len(), 3, "{:?}", forward.problems);
    assert!(forward.problems.iter().any(|p| matches!(
        p,
        super::Problem::FmtGap {
            index: 2,
            missing: 1,
            ..
        }
    )));
    let types = facts_named(&check, 1, "chosen");
    assert!(!types.is_empty());
    assert!(types.iter().all(|ty| ty == "std.Int"), "{types:?}");
    let types = facts_named(&check, 1, "ordered");
    assert!(!types.is_empty());
    // The earlier template interns its keys first; preserve TypeId order, not the
    // second template's source order, while sorting positional holes numerically.
    assert!(
        types.iter().all(|ty| ty
            == "std.Fmt[Never, {Unknown, Unknown, earlier_hole: Unknown, later_hole: Unknown}]"),
        "{types:?}",
    );
}

/// The names of `func`'s source variables live at each of its blocks that `select`
/// picks
fn live_at(
    source: &str,
    func: &str,
    select: impl Fn(&Ir, &Layout, BlockId) -> bool,
) -> Vec<Vec<String>> {
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source);
    assert!(!unit.failed, "the fixture doesn't compile");
    let mut builder = Builder::new();
    builder.unit(&unit).unwrap();
    let check = builder.check();
    let ir = check.cfgs[0]
        .as_ref()
        .expect("a unit with source is lowered");
    let layout = Layout::new(ir);
    let (id, _) = (ir.funcs())
        .find(|(_, data)| match data.kind {
            FuncKind::Decl(decl) => check.tables.qualified(decl).ends_with(&format!(".{func}")),
            _ => false,
        })
        .expect("the function is lowered");
    let vars = &layout.vars[id.index()];
    let blocks: Vec<Vec<String>> = (ir.blocks())
        .filter(|&(block, data)| data.func == id && select(ir, &layout, block))
        .map(|(block, _)| {
            (layout.live[block.index()].iter_ones())
                .filter_map(|slot| match ir.var(vars[slot]).origin {
                    Origin::Source(span) => Some(check.tables.text(layout.unit, span).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .collect();
    assert!(!blocks.is_empty(), "no block is selected");
    blocks
}

/// A loop's head, where a back edge joins
fn head(_ir: &Ir, layout: &Layout, block: BlockId) -> bool {
    layout.blocks.widens[block.index()]
}

/// Only what an iteration reads from an earlier one is live at a loop's head
#[test]
fn per_iteration_bindings_are_dead_at_heads() {
    let heads = live_at(
        "
def f r @ Array[Int | Str]
  let total = 0
  for e = r
    let k = do e
    match e
      Int(n) do total = (total + n)
      s do k()
  total
",
        "f",
        head,
    );
    for live in heads {
        assert!(live.contains(&"total".to_owned()), "{live:?}");
        for dead in ["e", "k", "n", "s"] {
            assert!(!live.contains(&dead.to_owned()), "{live:?}");
        }
    }
}

/// Where a variable may have escaped is decided before flow, so assigning it
/// kills it even once it may have
#[test]
fn escaped_assignments_kill() {
    let heads = live_at(
        "
def f r
  let c = 0
  let k = do c
  for e = r
    c = e
    k()
",
        "f",
        head,
    );
    for live in heads {
        assert!(!live.contains(&"c".to_owned()), "{live:?}");
    }
}

/// A `finally` body continues to where it was entered for
#[test]
fn live_through_finally() {
    let ends = live_at(
        "
def f g
  let x = 1
  try
    g()
  finally
    g()
  x
",
        "f",
        |ir, _, block| matches!(ir.block(block).terminal, Terminal::EndFinally),
    );
    for live in ends {
        assert!(live.contains(&"x".to_owned()), "{live:?}");
    }
}

/// A step that throws continues at its handler
#[test]
fn live_into_handlers() {
    let bodies = live_at(
        "
def f g
  let x = 1
  try
    g()
  catch _
    return x
  nil
",
        "f",
        |ir, _, block| ir.block(block).handler.is_some(),
    );
    for live in bodies {
        assert!(live.contains(&"x".to_owned()), "{live:?}");
    }
}
