use std::path::Path;

use crate::{Config, typeck::Builder};

/// Lower a script, which must be valid and keep the operand stack balanced
fn lower(source: &str) {
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
