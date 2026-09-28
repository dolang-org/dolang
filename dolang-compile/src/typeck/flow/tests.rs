use std::path::Path;

use super::Flow;
use crate::{
    Config,
    typeck::{Builder, r#type::UnitId},
};

/// Analyze a script both in queue order and in reverse, which must agree
fn agree(source: &str) {
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
    let ir = &check.cfgs[UnitId::from_index(0).index()];
    let forward = Flow::new(ir, &check.db, &check.tables, false).analyze();
    let reversed = Flow::new(ir, &check.db, &check.tables, true).analyze();
    assert_eq!(forward, reversed);
    assert_eq!(forward, check.flows[0]);
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
