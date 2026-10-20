use std::path::Path;

use super::Flow;
use crate::{
    Config,
    typeck::{
        Builder, Check,
        cfg::{BlockId, FuncKind, Origin, Terminal},
        r#type::UnitId,
    },
};

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
    let ir = check.cfgs[UnitId::from_index(0).index()]
        .as_ref()
        .expect("a unit with source is lowered");
    let forward = Flow::new(ir, &check.forks[0], &check.tables, false).analyze();
    let reversed = Flow::new(ir, &check.forks[0], &check.tables, true).analyze();
    assert_eq!(forward, reversed);
    assert_eq!(Some(forward), check.flows[0]);
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
                let types: Vec<_> = flow.facts.iter()
                    .filter(|(span, _)| check.tables.text(UnitId::from_index(0), **span) == name)
                    .map(|(_, fact)| check.tables.render_type(&check.forks[0], fact.ty))
                    .collect();
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
    let ir = check.cfgs[1].as_ref().unwrap();
    let forward = Flow::new(ir, &check.forks[1], &check.tables, false).analyze();
    let reversed = Flow::new(ir, &check.forks[1], &check.tables, true).analyze();
    assert_eq!(forward, reversed);
    assert_eq!(Some(&forward), check.flows[1].as_ref());
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
    let types: Vec<_> = forward
        .facts
        .iter()
        .filter(|(span, _)| check.tables.text(UnitId::from_index(1), **span) == "chosen")
        .map(|(_, fact)| check.tables.render_type(&check.forks[0], fact.ty))
        .collect();
    assert!(!types.is_empty());
    assert!(types.iter().all(|ty| ty == "std.Int"), "{types:?}");
    let types: Vec<_> = forward
        .facts
        .iter()
        .filter(|(span, _)| check.tables.text(UnitId::from_index(1), **span) == "ordered")
        .map(|(_, fact)| check.tables.render_type(&check.forks[1], fact.ty))
        .collect();
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
    select: impl Fn(&Flow<'_, '_>, BlockId) -> bool,
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
    let flow = Flow::new(ir, &check.forks[0], &check.tables, false);
    let (id, _) = (ir.funcs())
        .find(|(_, data)| match data.kind {
            FuncKind::Decl(decl) => check.tables.qualified(decl).ends_with(&format!(".{func}")),
            _ => false,
        })
        .expect("the function is lowered");
    let vars = &flow.funcs[id.index()].vars;
    let blocks: Vec<Vec<String>> = (ir.blocks())
        .filter(|&(block, data)| data.func == id && select(&flow, block))
        .map(|(block, _)| {
            (flow.live[block.index()].iter_ones())
                .filter_map(|slot| match ir.var(vars[slot]).origin {
                    Origin::Source(span) => Some(check.tables.text(flow.unit, span).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .collect();
    assert!(!blocks.is_empty(), "no block is selected");
    blocks
}

/// A loop's head, where a back edge joins
fn head(flow: &Flow<'_, '_>, block: BlockId) -> bool {
    flow.blocks.widens[block.index()]
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

/// Assigning a variable that may have escaped reads whether it has, so it's live
/// before the assignment though nothing reads its value
#[test]
fn escaped_assignments_read() {
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
        assert!(live.contains(&"c".to_owned()), "{live:?}");
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
        |flow, block| matches!(flow.ir.block(block).terminal, Terminal::EndFinally),
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
        |flow, block| flow.ir.block(block).handler.is_some(),
    );
    for live in bodies {
        assert!(live.contains(&"x".to_owned()), "{live:?}");
    }
}
