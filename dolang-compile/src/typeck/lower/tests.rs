use std::path::Path;

use crate::{
    Config,
    typeck::{
        Builder,
        cfg::{BlockId, ExprKind, FuncId, Ir, Origin, Step, Terminal, VarFlags, VarId},
    },
};

/// Lower a script, which must be valid and keep the operand stack balanced
fn lower(source: &str) {
    lower_with(source, |_| {});
}

fn lower_with(source: &str, inspect: impl FnOnce(&Ir)) {
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
    let ir = check.cfgs[0]
        .as_ref()
        .expect("a unit with source is lowered");
    assert_eq!(ir.validate(), Ok(()));
    ir.check_stack_depths();
    inspect(ir);
}

/// Short circuits as call arguments, nested in each other, and as conditions and
/// a `throw`'s value give valid graphs with a balanced stack
#[test]
fn short_circuit_stacks() {
    lower(
        "
let f = (do |a b c| a)
let x = 1
f $x (x && x.y) (x || 2)
let a b c = [1, 2, 3]
let y = ((a && b) || c && !(a || b))
def g x y
  while (x && y)
    if (y || x)
      break
    continue
  throw (x && y)
",
    );
}

/// Where a function is created: the step whose expressions hold its lambda
fn creation(ir: &Ir, func: FuncId) -> Option<(BlockId, usize)> {
    ir.blocks().find_map(|(id, block)| {
        block
            .steps
            .iter()
            .position(|step| {
                let value = match step {
                    Step::Assign(target) => target.value(),
                    Step::Let { value, .. }
                    | Step::Default { value, .. }
                    | Step::Eval(value)
                    | Step::Push(value) => value,
                    _ => return false,
                };
                let mut found = false;
                value.walk(&mut |expr| {
                    found |= matches!(expr.kind, ExprKind::Lambda(f) if f == func)
                });
                found
            })
            .map(|index| (id, index))
    })
}

/// The `Capture` step naming a function
fn capture(ir: &Ir, func: FuncId) -> (BlockId, usize) {
    ir.blocks()
        .find_map(|(id, block)| {
            (block.steps.iter())
                .position(|step| matches!(step, Step::Capture(funcs) if funcs.contains(&func)))
                .map(|index| (id, index))
        })
        .expect("every nested function is captured")
}

/// A function's creation follows the `Capture` naming it, in the same block: after
/// a short circuit's test, and in a nested block
#[test]
fn captures_precede_creation() {
    lower_with(
        "
def f x
  x
let x = 1
let g = do x
let y = (x && f(do x))
let h = do
  let inner = do x
  inner
",
        |ir| {
            let mut lambdas = 0;
            for (id, func) in ir.funcs() {
                if func.parent.is_none() {
                    continue;
                }
                let Some((block, index)) = creation(ir, id) else {
                    continue;
                };
                lambdas += 1;
                let (captured, at) = capture(ir, id);
                assert_eq!(
                    captured,
                    block,
                    "f{} is created in its capture's block",
                    id.index()
                );
                assert!(
                    at < index,
                    "f{}'s capture precedes its creation",
                    id.index()
                );
            }
            assert_eq!(lambdas, 5);
        },
    );
}

/// A comprehension's item creates a function in its item block, though the
/// lambda stays in the collection's tree, evaluated once the comprehension ends
#[test]
fn comprehension_captures() {
    lower_with(
        "
let ys = $
  for i = [1, 2]
    - (do i)
",
        |ir| {
            let (id, _) = (ir.funcs())
                .find(|(_, func)| func.parent.is_some())
                .unwrap();
            let (block, _) = capture(ir, id);
            let body = (ir.blocks())
                .find_map(|(_, block)| match block.terminal {
                    Terminal::Next { body, .. } => Some(body),
                    _ => None,
                })
                .unwrap();
            assert_eq!(block, body);
            let (created, _) = creation(ir, id).unwrap();
            assert_ne!(created, block);
        },
    );
}

/// A class statement creates its methods and field initializers, named by one
/// `Capture` step
#[test]
fn class_captures() {
    lower_with(
        "
def g()
  1
class A
  pub field y = (g())
  pub def m self
    self.y
  pub def n _self
    g()
",
        |ir| {
            let members: Vec<_> = (ir.funcs())
                .filter(|(_, func)| func.parent.is_some())
                .filter(|&(id, _)| creation(ir, id).is_none())
                .map(|(id, _)| id)
                .collect();
            assert_eq!(members.len(), 3);
            let (block, index) = capture(ir, members[0]);
            let Step::Capture(funcs) = &ir.block(block).steps[index] else {
                unreachable!()
            };
            assert_eq!(funcs, &members);
        },
    );
}

/// A variable escapes through the function its owner creates, however deeply
/// it's captured, even one another function assigns
#[test]
fn escapes() {
    lower_with(
        "
let v = 1
let w = 1
let outer = do
  let inner = do
    w = v
  inner
",
        |ir| {
            let outer = (ir.funcs())
                .find(|(_, func)| func.parent.is_some_and(|p| ir.func(p).parent.is_none()))
                .map(|(id, _)| id)
                .unwrap();
            let inner = (ir.funcs())
                .find(|(_, func)| func.parent == Some(outer))
                .map(|(_, func)| func)
                .unwrap();
            let outer = ir.func(outer);
            assert_eq!(outer.escapes.len(), 2);
            let volatile = |var: &&VarId| ir.var(**var).flags.contains(VarFlags::VOLATILE);
            assert_eq!(outer.escapes.iter().filter(volatile).count(), 1);
            assert!(inner.escapes.is_empty());
            assert!(outer.escapes.iter().all(|var| inner.captures.contains(var)));
        },
    );
}

/// A function caches the variables it captures, at its entry, unless another
/// function assigns them
#[test]
fn cached() {
    lower_with(
        "
let v = 1
let w = 1
let outer = do
  let inner = do
    w = v
  inner
",
        |ir| {
            let cached = |func: FuncId| match ir.block(ir.func(func).entry).steps.first() {
                Some(Step::Cache(vars)) => vars.clone(),
                _ => Vec::new(),
            };
            let outer = (ir.funcs())
                .find(|(_, func)| func.parent.is_some_and(|p| ir.func(p).parent.is_none()))
                .map(|(id, _)| id)
                .unwrap();
            let inner = (ir.funcs())
                .find(|(_, func)| func.parent == Some(outer))
                .map(|(id, _)| id)
                .unwrap();
            let volatile = |var: &VarId| ir.var(*var).flags.contains(VarFlags::VOLATILE);
            let stable: Vec<_> = (ir.func(outer).escapes.iter().copied())
                .filter(|var| !volatile(var))
                .collect();
            assert!(cached(outer).is_empty());
            assert_eq!(stable.len(), 1);
            assert_eq!(cached(inner), stable);
        },
    );
}

/// Only a `def`'s or `class`'s variable is hoisted
#[test]
fn hoisted() {
    let source = "
def f()
  1
class C
  pub field y = 1
let v = 1
for i = [1]
  let w = i
";
    lower_with(source, |ir| {
        let (_, top) = (ir.funcs())
            .find(|(_, func)| func.parent.is_none())
            .unwrap();
        let mut hoisted: Vec<_> = (top.vars.iter())
            .filter_map(|&var| {
                let var = ir.var(var);
                let Origin::Source(span) = var.origin else {
                    return None;
                };
                let name = &source[span.start as usize..span.end as usize];
                Some((name, var.flags.contains(VarFlags::HOISTED)))
            })
            .collect();
        hoisted.sort();
        assert_eq!(
            hoisted,
            [
                ("C", true),
                ("f", true),
                ("i", false),
                ("v", false),
                ("w", false)
            ]
        );
    });
}

/// A variable its owner assigns where it may have escaped isn't cached: a
/// reassignment after a capture counts, an assignment before one doesn't, and
/// rebinding a `for` variable makes a fresh cell
#[test]
fn uncached() {
    let source = "
let a = 1
let b = 1
b = 2
let f = do
  (a, b)
a = 2
for i = [1, 2]
  let g = (do i)
for j = [1, 2]
  let h = (do j)
  j = 3
";
    lower_with(source, |ir| {
        let (_, top) = (ir.funcs())
            .find(|(_, func)| func.parent.is_none())
            .unwrap();
        let name = |var: VarId| match ir.var(var).origin {
            Origin::Source(span) => Some(&source[span.start as usize..span.end as usize]),
            _ => None,
        };
        let mut uncached: Vec<_> = (top.vars.iter())
            .filter(|&&var| ir.var(var).flags.contains(VarFlags::INTERPROCEDURAL))
            .filter_map(|&var| Some((name(var)?, ir.var(var).flags.contains(VarFlags::UNCACHED))))
            .collect();
        uncached.sort();
        assert_eq!(
            uncached,
            [("a", true), ("b", false), ("i", false), ("j", true)]
        );
        let (_, f) = (ir.funcs())
            .find(|(_, func)| func.captures.iter().any(|&var| name(var) == Some("a")))
            .unwrap();
        let Some(Step::Cache(cached)) = ir.block(f.entry).steps.first() else {
            panic!("`f` caches `b`");
        };
        let cached: Vec<_> = cached.iter().filter_map(|&var| name(var)).collect();
        assert_eq!(cached, ["b"]);
    });
}
