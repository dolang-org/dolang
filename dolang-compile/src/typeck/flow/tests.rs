use std::path::Path;

use super::Flow;
use crate::{
    Config,
    typeck::{Builder, Check, r#type::UnitId},
};

/// Analyze a script both in queue order and in reverse, which must agree
fn agree(source: &str) {
    agree_with(source, |_| {});
}

fn agree_with(source: &str, inspect: impl FnOnce(&Check<'_>)) {
    let mut config = Config::new();
    config.typecheck(true);
    let unit = config.unit(Path::new("test.dol"), source.as_bytes());
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
    let forward = Flow::new(ir, &check.db, &check.tables, false).analyze();
    let reversed = Flow::new(ir, &check.db, &check.tables, true).analyze();
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
                    .map(|(_, fact)| check.tables.render_type(&check.db, fact.ty))
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
fn templates_are_order_independent() {
    let mut config = Config::new();
    config
        .typecheck(true)
        .mode(crate::Mode::Module { name: "std" });
    let std = config.unit(
        Path::new("std.dol"),
        include_bytes!("../../../../dolang/stub/std.dol"),
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
    let script = config.unit(Path::new("test.dol"), source.as_bytes());
    assert!(!script.failed);
    let mut builder = Builder::new();
    builder.unit(&std).unwrap();
    builder.unit(&script).unwrap();
    let check = builder.check();
    let ir = check.cfgs[1].as_ref().unwrap();
    let forward = Flow::new(ir, &check.db, &check.tables, false).analyze();
    let reversed = Flow::new(ir, &check.db, &check.tables, true).analyze();
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
        .map(|(_, fact)| check.tables.render_type(&check.db, fact.ty))
        .collect();
    assert!(!types.is_empty());
    assert!(types.iter().all(|ty| ty == "std.Int"), "{types:?}");
}
