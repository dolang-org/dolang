use std::path::Path;

use crate::{
    Config,
    typeck::{
        Builder,
        cfg::{BlockId, ExprKind, FuncId, Ir, Step, Terminal},
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
/// it's captured, unless another function assigns it
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
            assert_eq!(outer.escapes.len(), 1);
            assert!(!ir.var(outer.escapes[0]).volatile);
            assert!(inner.escapes.is_empty());
            assert!(inner.captures.contains(&outer.escapes[0]));
        },
    );
}
