use std::{
    panic::{self, AssertUnwindSafe},
    path::Path,
};

use super::{Invalid, encode, read};
use crate::{
    Config, ErrorKind, Mode, Unit,
    source::Span,
    typeck::{
        self, Builder, Typelib,
        elab::{
            Referent, Target,
            surface::{Name, StrId},
        },
    },
};

const MODULE: &str = "
pub class Box[T]
  pub field value @ T

  def (init) self value @ T
    self.value = value

  pub def get self -> T
    self.value

pub @class Shape
  pub def area self -> Int

pub @let Pair[T @ Int = Int] = Tuple[T, T]

@def double x@Int -> Int
@def double x@Str -> Str
pub def double x
  let twice = do |y| y + y
  class Local
    field z @ Int = 0
  twice $x

pub def first[T] items @ Array[T] :key @ Str = \"a\" *rest @ Int -> T
  items[0]

pub let limit @ Int = 1
pub let loose = 2
";

const SCRIPT: &str = "
import lib:
  - Box
  - double
  - first
  - @Pair
  - @Shape
  - limit
  - loose
let b @ Box[Int] = Box 1
let p @ Pair[Int] = (1, 2)
let s @ Shape = nil
double 2
first [1]
let l @ Str = limit
let m @ Str = loose
";

fn compile<'a>(source: &'a str, mode: Mode<'a>) -> Unit<'a> {
    let mut config = Config::new();
    config.typecheck(true).mode(mode);
    let unit = config.unit(Path::new("lib.dol"), source.as_bytes());
    let diags: Vec<_> = (unit.diagnostics())
        .map(|diag| diag.message().to_string())
        .collect();
    assert!(!unit.failed, "the fixture doesn't compile: {diags:?}");
    unit
}

fn typelib() -> Vec<u8> {
    typeck::typelib(&compile(MODULE, Mode::Module { name: "lib".into() })).unwrap()
}

/// Check the script against a typelib, returning its diagnostics.
fn check(typelib: &[u8]) -> Result<Vec<String>, crate::Error> {
    let script = compile(SCRIPT, Mode::Script);
    let typelib = Typelib::read(typelib)?;
    let mut builder = Builder::new();
    builder.typelib(&typelib)?;
    builder.unit(&script).unwrap();
    let check = builder.check();
    check.smoke();
    Ok(check
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .collect())
}

#[test]
fn round_trip() {
    let bytes = typelib();
    let harvest = read(&bytes).unwrap();
    assert!(
        harvest.decls.iter().all(|decl| decl.name.is_some()),
        "the bodies' declarations are left out"
    );
    assert_eq!(encode(harvest), bytes);
}

#[test]
fn checks_against_typelib() {
    let script = compile(SCRIPT, Mode::Script);
    let module = compile(MODULE, Mode::Module { name: "lib".into() });
    let mut builder = Builder::new();
    builder.unit(&module).unwrap();
    builder.unit(&script).unwrap();
    let from_source = builder.check();
    let script_unit =
        |diag: &&crate::diag::Diag| diag.span().unit().map(|unit| unit.index()) == Some(1);
    let expected: Vec<_> = (from_source.diagnostics())
        .filter(script_unit)
        .map(|diag| diag.message().to_string())
        .collect();
    assert_eq!(check(&typelib()).unwrap(), expected);
}

#[test]
fn script_has_no_typelib() {
    let error = typeck::typelib(&compile(SCRIPT, Mode::Script)).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NotModule));
}

#[test]
fn duplicate_module() {
    let bytes = typelib();
    let typelib = Typelib::read(&bytes).unwrap();
    let module = compile(MODULE, Mode::Module { name: "lib".into() });
    let mut builder = Builder::new();
    builder.unit(&module).unwrap();
    let error = builder.typelib(&typelib).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::DuplicateModule));
}

#[test]
fn describes_module() {
    let source = "
import json time uuid
import geometry:
  - @Point
pub @let When = time.Instant
pub def at p @ Point -> When
  let _id @ uuid.Uuid = nil
  nil
pub let unused @ json.Value = nil
";
    let bytes = typeck::typelib(&compile(source, Mode::Module { name: "lib".into() })).unwrap();
    let typelib = Typelib::read(&bytes).unwrap();
    assert_eq!(typelib.module(), "lib");
    assert_eq!(typelib.path(), Path::new("lib.dol"));
    // A name written only in a body is not on the surface
    assert_eq!(typelib.imports(), ["geometry", "json", "time"]);
}

#[test]
fn bad_header() {
    let mut bytes = typelib();
    bytes[1] ^= 1;
    assert!(matches!(read(&bytes), Err(Invalid::Header)));
    assert!(matches!(read(b""), Err(Invalid::Header)));
}

#[test]
fn other_version() {
    let mut bytes = typelib();
    // The version follows the 8-byte magic
    bytes[8] += 1;
    assert!(matches!(read(&bytes), Err(Invalid::Version(_))));
    let error = check(&bytes).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::Typelib));
}

#[test]
fn truncated() {
    let bytes = typelib();
    for len in 0..bytes.len() {
        assert!(read(&bytes[..len]).is_err(), "truncated to {len} bytes");
    }
}

#[test]
fn trailing() {
    let mut bytes = typelib();
    bytes.push(0);
    assert!(matches!(read(&bytes), Err(Invalid::Malformed(_))));
}

#[test]
fn out_of_range() {
    let bytes = typelib();
    let mut harvest = read(&bytes).unwrap();
    let strings = harvest.strings.len();
    harvest.decls[0].name = Some(Name {
        text: StrId::from_index(strings),
        ..harvest.decls[0].name.unwrap()
    });
    assert!(matches!(
        read(&encode(harvest)),
        Err(Invalid::Malformed("an ID is out of range"))
    ));
}

#[test]
fn exported_variables() {
    let bytes = typelib();
    let harvest = read(&bytes).unwrap();
    let annotated = |name: &str| {
        let (_, target) = &harvest.exports[name];
        let Target::Local(Referent::Value(value)) = target else {
            panic!("`{name}` is exported as a variable");
        };
        harvest.values[&value.span].is_some()
    };
    assert!(annotated("limit"));
    assert!(!annotated("loose"));
    assert_eq!(harvest.values.len(), 2);

    let mut unlisted = read(&bytes).unwrap();
    unlisted.values.clear();
    assert!(matches!(
        read(&encode(unlisted)),
        Err(Invalid::Malformed("an exported variable is not listed"))
    ));

    let mut stray = read(&bytes).unwrap();
    stray.values.insert(Span::default(), None);
    assert!(matches!(
        read(&encode(stray)),
        Err(Invalid::Malformed("a listed variable is not exported"))
    ));
}

/// Corrupting any byte must make reading fail, or leave a typelib the checker
/// handles without panicking.
#[test]
#[cfg_attr(miri, ignore = "slow")]
fn corruption() {
    let bytes = typelib();
    let mut read_ok = 0;
    let mut panics = Vec::new();
    for index in 0..bytes.len() {
        for mask in [0x01, 0x80, 0xff] {
            let mut corrupt = bytes.clone();
            corrupt[index] ^= mask;
            match panic::catch_unwind(AssertUnwindSafe(|| check(&corrupt).is_ok())) {
                Ok(ok) => read_ok += usize::from(ok),
                Err(_) => panics.push((index, mask)),
            }
        }
    }
    assert!(
        panics.is_empty(),
        "corrupting these bytes panics: {panics:x?}"
    );
    assert!(
        read_ok > 0,
        "some corruptions are read, so the checker is exercised"
    );
}
