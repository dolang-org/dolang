use std::{collections::HashMap, path::Path};

use crate::{
    Config,
    typeck::{
        Builder,
        cfg::{Against, BlockId, Expr, ExprKind, Ir, Pattern, Step, Tag, Target, Terminal},
        r#type::UnitId,
    },
};

/// Lower a script and dump its graph, which must be valid and keep the operand
/// stack balanced
fn lower(source: &str) -> String {
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
    let ir = check.cfgs[0]
        .as_ref()
        .expect("a unit with source is lowered");
    assert_eq!(ir.validate(), Ok(()));
    stack_depths(ir);
    ir.dump(&check.db, |span| {
        check.tables.text(UnitId::from_index(0), span)
    })
}

fn operands(expr: &Expr) -> usize {
    let mut count = 0;
    expr.walk(&mut |expr| count += matches!(expr.kind, ExprKind::Operand) as usize);
    count
}

fn pattern_operands(pattern: &Pattern) -> usize {
    let mut count = 0;
    pattern.walk(&mut |expr| count += operands(expr));
    count
}

/// Check that each block is entered at one stack depth, that nothing pops more than
/// is there, and that a statement boundary a jump or `finally` leaves from has an
/// empty stack. A handler is entered with the exception alone on the stack.
fn stack_depths(ir: &Ir) {
    struct Walk {
        depths: HashMap<BlockId, usize>,
        work: Vec<BlockId>,
    }
    impl Walk {
        fn enter(&mut self, block: BlockId, depth: usize) {
            match self.depths.insert(block, depth) {
                Some(old) => assert_eq!(old, depth, "b{} entered at two depths", block.index()),
                None => self.work.push(block),
            }
        }
    }
    let mut walk = Walk {
        depths: HashMap::new(),
        work: Vec::new(),
    };
    for (_, func) in ir.funcs() {
        walk.enter(func.entry, 0);
    }
    while let Some(id) = walk.work.pop() {
        let block = ir.block(id);
        let mut depth = walk.depths[&id];
        let mut enter = |block, depth| walk.enter(block, depth);
        let pop = |depth: &mut usize, count: usize| {
            *depth = depth
                .checked_sub(count)
                .unwrap_or_else(|| panic!("b{} pops an empty stack", id.index()));
        };
        if let Some(handler) = block.handler {
            enter(handler, 1);
        }
        for step in &block.steps {
            match step {
                Step::Let { pattern, value } => {
                    pop(&mut depth, operands(value) + pattern_operands(pattern))
                }
                Step::Assign { target, value } => {
                    let target = match target {
                        Target::Var(_) => 0,
                        Target::Field { object, .. } => operands(object),
                        Target::Index { object, index, .. } => operands(object) + operands(index),
                    };
                    pop(&mut depth, target + operands(value));
                }
                Step::Default { value, .. } | Step::Eval(value) => pop(&mut depth, operands(value)),
                Step::Push(value) => {
                    pop(&mut depth, operands(value));
                    depth += 1;
                }
                Step::Dup => {
                    assert!(depth > 0, "b{} duplicates an empty stack", id.index());
                    depth += 1;
                }
                Step::Pop => pop(&mut depth, 1),
                Step::Assume(assume) => match &assume.against {
                    Against::Class(expr) | Against::Value(expr) => {
                        assert_eq!(operands(expr), 0)
                    }
                    Against::Type(_) | Against::Decl(_) => {}
                },
            }
        }
        let empty = |depth: usize| assert_eq!(depth, 0, "b{} leaves with a stack", id.index());
        match &block.terminal {
            Terminal::Branch(next) => enter(*next, depth),
            Terminal::If { cond, then, else_ } => {
                pop(&mut depth, operands(cond));
                enter(*then, depth);
                enter(*else_, depth);
            }
            Terminal::Unpack {
                pattern,
                value,
                then,
                else_,
            } => {
                pop(&mut depth, operands(value) + pattern_operands(pattern));
                enter(*then, depth);
                enter(*else_, depth);
            }
            Terminal::Catch { clauses, otherwise } => {
                for (class, _) in clauses {
                    pop(&mut depth, operands(class));
                }
                assert_eq!(depth, 1, "b{} dispatches the exception alone", id.index());
                for (_, clause) in clauses {
                    enter(*clause, depth);
                }
                enter(*otherwise, depth);
            }
            Terminal::Next {
                pattern,
                body,
                exit,
                ..
            } => {
                // A comprehension's loop may run above a command's earlier arguments
                pop(&mut depth, pattern_operands(pattern));
                enter(*body, depth);
                enter(*exit, depth);
            }
            Terminal::Throw(value) | Terminal::ReturnFrom { value, .. } => {
                pop(&mut depth, operands(value))
            }
            Terminal::Leave { entry, tag } => {
                empty(depth);
                enter(*entry, 0);
                if let Tag::Goto(next) = tag {
                    enter(*next, 0);
                }
            }
            Terminal::Guard { next, targets } => {
                empty(depth);
                enter(*next, 0);
                for target in targets {
                    enter(*target, 0);
                }
            }
            Terminal::Return | Terminal::EndFinally => empty(depth),
            Terminal::Escape | Terminal::Unreachable => {}
        }
    }
}

#[track_caller]
fn check(source: &str, expected: &str) {
    let actual = lower(source);
    assert!(
        actual.trim() == expected.trim(),
        "graph mismatch\n--- actual ---\n{actual}"
    );
}

/// Each short circuit pushes its left operand, and the call pops the results in
/// argument order. The callee and the first argument stay in the tree.
#[test]
fn short_circuits_in_call() {
    check(
        "
let f = (do |a b c| a)
let x = 1
f $x (x && x.y) (x || 2)
",
        "
f0 module: entry b0, exit b1, params (), bottom t7 t8 t9 t10 t11 t12
f1 decl0 in f0: entry b2, exit b3, params (a, b, c), signature (t7, t8, t9) <t10 >t11 -> t12, captures t7 t8 t9 t10 t11 t12!
b0 f0:
  let f = f1
  let x = 1
  push x
  dup
  if <pop> then b4 else b5
b1 f0:
  return
b2 f1:
  result1 = a
  goto b3
b3 f1:
  t12 = result1
  return
b4 f0:
  assume x != nil
  assume x != false
  pop
  push x.y
  goto b5
b5 f0:
  push x
  dup
  if <pop> then b7 else b6
b6 f0:
  pop
  push 2
  goto b7
b7 f0:
  result0 = f(x, <pop>, <pop>)
  goto b1
",
    );
}

/// The inner result is already on top of the stack, so it isn't pushed again
#[test]
fn nested_short_circuit_pushes_once() {
    check(
        "
let a b c = [1, 2, 3]
let x = ((a && b) || c)
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let (a, b, c) = array[1, 2, 3]
  push a
  dup
  if <pop> then b2 else b3
b1 f0:
  return
b2 f0:
  assume a != nil
  assume a != false
  pop
  push b
  goto b3
b3 f0:
  dup
  if <pop> then b5 else b4
b4 f0:
  pop
  push c
  goto b5
b5 f0:
  let x = <pop>
  result0 = x
  goto b1
",
    );
}

/// A condition's short circuits branch without the stack, while a `throw`'s value
/// uses it
#[test]
fn short_circuit_in_loop_condition_and_exits() {
    check(
        "
def f x y
  while (x && y)
    if (y || x)
      break
    continue
  throw (x && y)
",
        "
f0 module: entry b0, exit b1, params ()
f1 decl0 in f0: entry b2, exit b3, params (x, y)
b0 f0:
  let f = f1
  result0 = f
  goto b1
b1 f0:
  return
b2 f1:
  goto b4
b3 f1:
  return
b4 f1:
  if x then b8 else b5
b5 f1:
  push x
  dup
  if <pop> then b10 else b11
b6 f1:
  if y then b15 else b14
b7 f1:
  if y then b9 else b5
b8 f1:
  assume x != nil
  assume x != false
  goto b7
b9 f1:
  assume y != nil
  assume y != false
  goto b6
b10 f1:
  assume x != nil
  assume x != false
  pop
  push y
  goto b11
b11 f1:
  throw <pop>
b12 f1:
  goto b4
b13 f1:
  goto b5
b14 f1:
  if x then b16 else b12
b15 f1:
  assume y != nil
  assume y != false
  goto b13
b16 f1:
  assume x != nil
  assume x != false
  goto b13
",
    );
}

/// Each edge keeps its narrowing and result; only fallthrough writes `nil`.
#[test]
fn narrowing() {
    check(
        "
def f x
  if (type x Int)
    x
  else if (x == nil)
    1
  else if x
    2
",
        "
f0 module: entry b0, exit b1, params ()
f1 decl0 in f0: entry b2, exit b3, params (x)
b0 f0:
  let f = f1
  result0 = f
  goto b1
b1 f0:
  return
b2 f1:
  if std::type(x, std::Int) then b7 else b8
b3 f1:
  return
b4 f1:
  goto b3
b5 f1:
  if (x == nil) then b11 else b12
b6 f1:
  result1 = x
  goto b4
b7 f1:
  assume x <: class std::Int
  goto b6
b8 f1:
  assume x !<: class std::Int
  goto b5
b9 f1:
  if x then b15 else b13
b10 f1:
  result1 = 1
  goto b4
b11 f1:
  assume x == nil
  goto b10
b12 f1:
  assume x != nil
  goto b9
b13 f1:
  result1 = nil
  goto b4
b14 f1:
  result1 = 2
  goto b4
b15 f1:
  assume x != nil
  assume x != false
  goto b14
",
    );
}

#[test]
fn loops() {
    check(
        "
for k v = {a: 1}
  if (k == :a:)
    continue
  break
let i = 0
while (i < 3)
  i = (i + 1)
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let t1 = dict[a: 1]
  goto b2
b1 f0:
  return
b2 f0:
  next (k, v) in t1 then b4 else b3
b3 f0:
  let i = 0
  goto b5
b4 f0:
  if (k == :a:) then b10 else b11
b5 f0:
  if (i < 3) then b7 else b6
b6 f0:
  result0 = nil
  goto b1
b7 f0:
  i = (i + 1)
  goto b5
b8 f0:
  goto b3
b9 f0:
  goto b2
b10 f0:
  assume k == :a:
  goto b9
b11 f0:
  assume k != :a:
  goto b8
",
    );
}

/// The body's handler dispatches to the clauses, a catch-all last. Leaving the
/// body or a clause enters the `finally` through a trampoline per target, and an
/// exception escaping a clause runs it before being raised again.
#[test]
fn try_catch_finally() {
    check(
        "
import error
def f x
  try
    return (x + 1)
  catch error.Type: e
    e
  catch e
    nil
  finally
    x
",
        "
f0 module: entry b0, exit b1, params ()
f1 decl0 in f0: entry b2, exit b3, params (x)
b0 f0:
  let f = f1
  result0 = f
  goto b1
b1 f0:
  return
b2 f1:
  guard b4
b3 f1:
  return
b4 f1:
  goto b9
b5 f1:
  goto b3
b6 f1 depth 1:
  eval x
  end finally
b7 f1:
  pop
  leave to b6 then rethrow
b8 f1 handler b7:
  catch error::Type -> b10, else b11
b9 f1 handler b8:
  result1 = (x + 1)
  goto b13
b10 f1 handler b7:
  let e = <pop>
  result1 = e
  goto b12
b11 f1 handler b7:
  let e = <pop>
  result1 = nil
  goto b12
b12 f1:
  leave to b6 then b5
b13 f1:
  leave to b6 then b3
",
    );
}

/// Leaving two `finally`s chains a trampoline for each
#[test]
fn nested_finally() {
    check(
        "
for x = [1]
  try
    try
      break
    finally
      1
  finally
    2
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let t1 = array[1]
  goto b2
b1 f0:
  return
b2 f0:
  next x in t1 then b4 else b3
b3 f0:
  result0 = nil
  goto b1
b4 f0:
  guard b5
b5 f0:
  goto b9
b6 f0:
  goto b2
b7 f0 depth 1:
  eval 2
  end finally
b8 f0:
  pop
  leave to b7 then rethrow
b9 f0 handler b8:
  goto b13
b10 f0 handler b8:
  goto b14
b11 f0 handler b8 depth 1:
  eval 1
  end finally
b12 f0 handler b8:
  pop
  leave to b11 then rethrow
b13 f0 handler b12:
  goto b16
b14 f0:
  leave to b7 then b6
b15 f0:
  leave to b7 then b3
b16 f0 handler b8:
  leave to b11 then b15
",
    );
}

/// A static field is evaluated before the class exists, and assigned after
#[test]
fn static_field() {
    check(
        "
class C
  #[static]
  pub field n = (1 + 2)
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  eval std::static
  let t2 = (1 + 2)
  let C = class0
  C.n = t2
  result0 = C
  goto b1
b1 f0:
  return
",
    );
}

/// Interpolations are `FmtValue`s, a `t"..."` sequence binds a bare one to an
/// empty specification, and a `${#...}` is a `FmtParam`
#[test]
fn formatting() {
    check(
        r#"
let x = 1
let w = 4
let s = "a${x:$w.2}b"
let t = t"a${x}b$x${#0:$w}"
let b = b"\x01$(b"\x02")"
"#,
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let x = 1
  let w = 4
  let s = concat(\"a\", fmt_value(x, width: w, precision: 2), \"b\")
  let t = fmt(\"a\", fmt_value(x), \"b\", fmt_value(x), fmt_param(width: w))
  let b = bin_concat(<bin>, <bin>)
  result0 = b
  goto b1
b1 f0:
  return
",
    );
}

/// An `if let` filter binds its pattern in the branch
#[test]
fn comprehension_pattern_filter() {
    check(
        "
let ps = [[1, 2]]
let xs = $
  for p = ps
    if let a b = p
      - (a + b)
",
        "
f0 module: entry b0, exit b1, params (), bottom p a b t7 t8
b0 f0:
  let ps = array[array[1, 2]]
  let t2 = ps
  goto b2
b1 f0:
  return
b2 f0:
  next p in t2 then b4 else b3
b3 f0:
  let xs = array[for {if {(t7 + t8)} else {}}]
  result0 = xs
  goto b1
b4 f0:
  unpack (a, b) = p then b6 else b5
b5 f0:
  goto b3
b6 f0:
  t7 = a
  t8 = b
  goto b5
",
    );
}

/// A comprehension's loop and filter are lowered to blocks before the statement,
/// with no back edge, and the values they produce go in variables starting at
/// bottom. Short circuits in them narrow as anywhere else.
#[test]
fn comprehensions() {
    check(
        "
let xs = [1, 2]
let g = (do |...a| a)
let ys = $
  for x = xs
    if (x && x > 1)
      - (x || 0)
g $ys
  - 1
  key: 2
",
        "
f0 module: entry b0, exit b1, params (), bottom x t8 t9 t10 t11 t12
f1 decl0 in f0: entry b2, exit b3, params (Mixed...a), signature (t9) <t10 >t11 -> t12, captures t9 t10 t11 t12!
b0 f0:
  let xs = array[1, 2]
  let g = f1
  let t3 = xs
  goto b4
b1 f0:
  return
b2 f1:
  result1 = a
  goto b3
b3 f1:
  t12 = result1
  return
b4 f0:
  next x in t3 then b6 else b5
b5 f0:
  let ys = array[for {if {t8} else {}}]
  result0 = g(ys, 1, key: 2)
  goto b1
b6 f0:
  if x then b10 else b7
b7 f0:
  goto b5
b8 f0:
  push x
  dup
  if <pop> then b12 else b11
b9 f0:
  if (x > 1) then b8 else b7
b10 f0:
  assume x != nil
  assume x != false
  goto b9
b11 f0:
  pop
  push 0
  goto b12
b12 f0:
  t8 = <pop>
  goto b7
",
    );
}

/// Nested loops and filters with `elif` and `else`. Only variable reads go in
/// variables: constants, lambdas, calls, strings and nested collections stay in
/// the tree.
#[test]
fn comprehension_nesting() {
    check(
        "
let xs = [1, 2]
let ys = $
  for x = xs
    for y = xs
      - [x, y]
      - $str(t\"$x-$y\")
    if (x > 1)
      - 1
    else if (x > 0)
      - (do x)
    else
      - $x
",
        "
f0 module: entry b0, exit b1, params (), bottom x t5 y t7 t8 t9 t10 t12 t13 t14 t15
f1 decl0 in f0: entry b13, exit b14, params (), signature () <t13 >t14 -> t15, captures t13 t14 t15! x
b0 f0:
  let xs = array[1, 2]
  let t2 = xs
  goto b2
b1 f0:
  return
b2 f0:
  next x in t2 then b4 else b3
b3 f0:
  let ys = array[for {for {array[t7, t8], std::str(fmt(fmt_value(t9), \"-\", fmt_value(t10)))}, if {1} else {if {f1} else {t12}}}]
  result0 = ys
  goto b1
b4 f0:
  let t5 = xs
  goto b5
b5 f0:
  next y in t5 then b7 else b6
b6 f0:
  if (x > 1) then b10 else b9
b7 f0:
  t7 = x
  t8 = y
  t9 = x
  t10 = y
  goto b6
b8 f0:
  goto b3
b9 f0:
  if (x > 0) then b12 else b11
b10 f0:
  goto b8
b11 f0:
  t12 = x
  goto b8
b12 f0:
  goto b8
b13 f1:
  result1 = x
  goto b14
b14 f1:
  t15 = result1
  return
",
    );
}

/// Keyed items, pairs and spreads in a call's comprehension, after an inline
/// argument whose short circuit stays on the stack below the loop's blocks
#[test]
fn comprehension_call_items() {
    check(
        "
let xs = [1, 2]
let a = 1
let b = 2
let g = (do |...rest| rest)
g (a || b)
  for x = xs
    k: 1
    (x): (x && a)
    ...xs
",
        "
f0 module: entry b0, exit b1, params (), bottom x t9 t10 t11 t12 t13 t14 t15
f1 decl0 in f0: entry b2, exit b3, params (Mixed...rest), signature (t12) <t13 >t14 -> t15, captures t12 t13 t14 t15!
b0 f0:
  let xs = array[1, 2]
  let a = 1
  let b = 2
  let g = f1
  push a
  dup
  if <pop> then b5 else b4
b1 f0:
  return
b2 f1:
  result1 = rest
  goto b3
b3 f1:
  t15 = result1
  return
b4 f0:
  pop
  push b
  goto b5
b5 f0:
  let t5 = xs
  goto b6
b6 f0:
  next x in t5 then b8 else b7
b7 f0:
  result0 = g(<pop>, for {k: 1, t9 => t10, ...t11})
  goto b1
b8 f0:
  t9 = x
  push x
  dup
  if <pop> then b9 else b10
b9 f0:
  assume x != nil
  assume x != false
  pop
  push a
  goto b10
b10 f0:
  t10 = <pop>
  t11 = xs
  goto b7
",
    );
}

/// A pattern's defaults are joined after it binds: after a `bind`, or on a
/// match's success edge, in a statement or a comprehension
#[test]
fn pattern_defaults() {
    check(
        "
let a = nil
bind [1]
  - x
  - y = (a || 1)
if bind [1]
  - m
  - n = 3
do
  m
let zs = $
  if bind [1]
    - u
    - v = (a || 5)
  do
    - $v
",
        "
f0 module: entry b0, exit b1, params (), bottom u v t9
b0 f0:
  let a = nil
  let (x, y) = array[1]
  push a
  dup
  if <pop> then b3 else b2
b1 f0:
  return
b2 f0:
  pop
  push 1
  goto b3
b3 f0:
  default y = <pop>
  unpack (m, n) = array[1] then b6 else b4
b4 f0:
  unpack (u, v) = array[1] then b9 else b7
b5 f0:
  eval m
  goto b4
b6 f0:
  default n = 3
  goto b5
b7 f0:
  let zs = array[if {t9} else {}]
  result0 = zs
  goto b1
b8 f0:
  t9 = v
  goto b7
b9 f0:
  push a
  dup
  if <pop> then b11 else b10
b10 f0:
  pop
  push 5
  goto b11
b11 f0:
  default v = <pop>
  goto b8
",
    );
}

/// A catch dispatch tries path classes in one `Catch`. A class that isn't a path
/// is evaluated in a dispatch of its own, and the catch-all goes last.
#[test]
fn catch_dispatch_chain() {
    check(
        "
import error
let a = nil
try
  a
catch error.Type: e
  1
catch (a || error.Type): e
  2
catch error.Value: e
  3
catch other
  4
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let a = nil
  goto b4
b1 f0:
  return
b2 f0:
  goto b1
b3 f0:
  catch error::Type -> b5, else b7
b4 f0 handler b3:
  result0 = a
  goto b2
b5 f0:
  let e = <pop>
  result0 = 1
  goto b2
b6 f0:
  let e = <pop>
  result0 = 2
  goto b2
b7 f0:
  push a
  dup
  if <pop> then b9 else b8
b8 f0:
  pop
  push error::Type
  goto b9
b9 f0:
  catch <pop> -> b6, error::Value -> b10, else b11
b10 f0:
  let e = <pop>
  result0 = 3
  goto b2
b11 f0:
  let other = <pop>
  result0 = 4
  goto b2
",
    );
}

#[test]
fn destructuring() {
    check(
        "
let a :b ...c = (1, b: 2)
bind (1, 2)
  - d
  - e = 3
for f g = [[1, 2]]
  f
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let (a, b: b, Mixed...c) = record[1, b: 2]
  let (d, e) = tuple[1, 2]
  default e = 3
  let t6 = array[array[1, 2]]
  goto b2
b1 f0:
  return
b2 f0:
  next (f, g) in t6 then b4 else b3
b3 f0:
  result0 = nil
  goto b1
b4 f0:
  eval f
  goto b2
",
    );
}

/// A variable a closure assigns is volatile; one only its owner assigns isn't, even
/// once captured
#[test]
fn captures() {
    check(
        "
let n = 0
let m = 1
let inc = do
  n = (n + m)
m = 2
m
",
        "
f0 module: entry b0, exit b1, params (), bottom t5 t6 t7
f1 decl0 in f0: entry b2, exit b3, params (), signature () <t5 >t6 -> t7, captures t5 t6 t7! n! m
b0 f0:
  let n = 0
  let m = 1
  let inc = f1
  m = 2
  result0 = m
  goto b1
b1 f0:
  return
b2 f1:
  n = (n + m)
  result1 = n
  goto b3
b3 f1:
  t7 = result1
  return
",
    );
}

/// A `do` block's signature has a variable of its parent's for each item written
/// without an annotation, and its exit joins its result into the result's. A def
/// and a method have none.
#[test]
fn lambda_signature() {
    check(
        "
def d x
  x
class C
  pub def m self
    nil
let f = do |a b@Int :k :j@Str *pos@Int **kw| a
let g = (do |x <Iter[Int]| -> Int x)
",
        "
f0 module: entry b0, exit b1, params (), bottom t18 t19 t20 t21 t22 t23 t24 t25
f1 decl0 in f0: entry b2, exit b3, params (x)
f2 decl2 in f0: entry b4, exit b5, params (self)
f3 decl3 in f0: entry b6, exit b7, params (a, b, k: k, j: j, Pos...pos, Key...kw), signature (t20, _, t21, _, _, t22) <t23 >t24 -> t25, captures t20 t21 t22 t23 t24 t25!
f4 decl4 in f0: entry b8, exit b9, params (x), signature (t18) <_ >t19 -> _, captures t18 t19
b0 f0:
  let d = f1
  let C = class1
  let f = f3
  let g = f4
  result0 = g
  goto b1
b1 f0:
  return
b2 f1:
  result1 = x
  goto b3
b3 f1:
  return
b4 f2:
  result2 = nil
  goto b5
b5 f2:
  return
b6 f3:
  result3 = a
  goto b7
b7 f3:
  t25 = result3
  return
b8 f4:
  result4 = x
  goto b9
b9 f4:
  return
",
    );
}

/// A `break` and a `return` out of a `do` block give the guard before the call
/// edges to the loop's exit and to a phantom return
#[test]
fn nonlocal_jumps() {
    check(
        "
def f xs
  for x = xs
    xs.each do |y|
      if y
        break
      return y
  nil
",
        "
f0 module: entry b0, exit b1, params ()
f1 decl0 in f0: entry b2, exit b3, params (xs), bottom t9 t10 t11 t12
f2 decl1 in f1: entry b8, exit b9, params (y), signature (t9) <t10 >t11 -> t12, captures t9 t10 t11 t12!
b0 f0:
  let f = f1
  result0 = f
  goto b1
b1 f0:
  return
b2 f1:
  let t4 = xs
  goto b4
b3 f1:
  return
b4 f1:
  next x in t4 then b6 else b5
b5 f1:
  result1 = nil
  goto b3
b6 f1:
  guard b7 | b13 | b5
b7 f1:
  eval xs.each(f2)
  goto b4
b8 f2:
  if y then b12 else b10
b9 f2:
  t12 = result2
  return
b10 f2:
  return from f1 y
b11 f2:
  escape
b12 f2:
  assume y != nil
  assume y != false
  goto b11
b13 f1:
  result1 = never
  goto b3
",
    );
}

/// Parameter defaults are entry steps; methods and field initializers are
/// functions nested where the class is
#[test]
fn defs_and_classes() {
    check(
        "
def f x y = (x + 1)
  y
class C
  pub field a = 1
  field b = []
  pub def get self
    self.#b
",
        "
f0 module: entry b0, exit b1, params ()
f1 decl0 in f0: entry b2, exit b3, params (x, y)
f2 decl3 in f0: entry b4, exit b5, params ()
f3 decl2 in f0: entry b6, exit b7, params (self)
b0 f0:
  let f = f1
  let C = class1
  result0 = C
  goto b1
b1 f0:
  return
b2 f1:
  default y = (x + 1)
  result1 = y
  goto b3
b3 f1:
  return
b4 f2:
  result2 = array[]
  goto b5
b5 f2:
  return
b6 f3:
  result3 = self.#b
  goto b7
b7 f3:
  return
",
    );
}

/// A sub-pattern's item binds a synthetic variable, which a later step unpacks.
/// Every level binds before any default joins, in item order.
#[test]
fn nested_patterns() {
    check(
        "
let x = [1, [2, [3]]]
let a (b (c d = 0)) = x
let f = do |p (q r = p)| [p, q, r]
for k (lo hi) = [x]
  [a, b, c, d, f, k, lo, hi]
",
        "
f0 module: entry b0, exit b1, params (), bottom t19 t20 t21 t22 t23
f1 decl0 in f0: entry b2, exit b3, params (p, t18), signature (t19, t20) <t21 >t22 -> t23, captures t19 t20 t21 t22 t23!
b0 f0:
  let x = array[1, array[2, array[3]]]
  let (a, t8) = x
  let (b, t9) = t8
  let (c, d) = t9
  default d = 0
  let f = f1
  let t7 = array[x]
  goto b4
b1 f0:
  return
b2 f1:
  let (q, r) = t18
  default r = p
  result1 = array[p, q, r]
  goto b3
b3 f1:
  t23 = result1
  return
b4 f0:
  next (k, t17) in t7 then b7 else b5
b5 f0:
  result0 = nil
  goto b1
b6 f0:
  eval array[a, b, c, d, f, k, lo, hi]
  goto b4
b7 f0:
  let (lo, hi) = t17
  goto b6
",
    );
}

/// A refutable sub-pattern is a further test, failing to where the pattern does
#[test]
fn nested_pattern_tests() {
    check(
        "
let v = {1, [2, 3], k: [4]}
if let a (b c) k: (d e = 0) = v
  [a, b, c, d, e]
else
  nil
",
        "
f0 module: entry b0, exit b1, params ()
b0 f0:
  let v = dict[1, array[2, 3], k: array[4]]
  unpack (a, t7, k: t8) = v then b6 else b3
b1 f0:
  return
b2 f0:
  goto b1
b3 f0:
  result0 = nil
  goto b2
b4 f0:
  result0 = array[a, b, c, d, e]
  goto b2
b5 f0:
  default e = 0
  goto b4
b6 f0:
  unpack (b, c) = t7 then b7 else b3
b7 f0:
  unpack (d, e) = t8 then b5 else b3
",
    );
}

#[test]
fn discarded_if_results() {
    let graph = lower("if true\n  1\nelse if false\n  2\nnil\n");
    assert!(graph.contains("eval 1"));
    assert!(graph.contains("eval 2"));
    assert!(!graph.contains("result0 = 1"));
    assert!(!graph.contains("result0 = 2"));
    assert_eq!(graph.matches("result0 = nil").count(), 1);
}

/// Optionality belongs to the unpack schema. Matching stays unchanged, and
/// collapsed defaults apply only along an optional ancestor's first alternatives.
#[test]
fn optional_patterns() {
    let graph = lower(
        r#"
let ?k: (a: a = 1 j: (Int(b = a) | Str(b = "dead"))) = ()
let Int(c = "dead") = 2
let f = do |?k: (p = 1 q = (p && 2))| (p, q)
if let ?k: Int(n = 0) = ()
  n
else
  nil
"#,
    );
    assert_eq!(graph.matches("default a = 1").count(), 1);
    assert_eq!(graph.matches("default b = a").count(), 1);
    assert!(graph.find("default a = 1") < graph.find("default b = a"));
    assert!(!graph.contains("default c"));
    assert!(!graph.contains("dead"));
    assert!(graph.contains("default p = 1"));
    assert!(graph.contains("default q = <pop>"));
    assert!(graph.contains("default n = 0"));
}
