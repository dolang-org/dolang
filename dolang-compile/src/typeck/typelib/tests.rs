use std::{
    panic::{self, AssertUnwindSafe},
    path::Path,
};

use super::{Header, Invalid, Surface, kept_spans, wire};
use crate::{
    Config, ErrorKind, Mode, Unit,
    source::Span,
    typeck::{
        Builder, Typelib,
        elab::{
            DeclNode, Harvest, Referent, Target, Value,
            surface::{Member, Name, ParamKind, StrId, TypeExpr},
        },
    },
};

/// Encode a harvest that is already a module's surface.
fn encode(harvest: Harvest<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    Surface::new(harvest).write(&mut bytes).unwrap();
    bytes
}

/// Write a unit's typelib from a check of it with an empty module of each name in
/// `imports`, and of `std`, which the prelude imports.
fn write(unit: &Unit<'_>, imports: &[&str]) -> Result<Vec<u8>, crate::Error> {
    let imported: Vec<_> = imports
        .iter()
        .chain(["std"].iter().filter(|_| !imports.contains(&"std")))
        .map(|&name| compile("", Mode::Module { name: name.into() }))
        .collect();
    let mut builder = Builder::new();
    builder.typelibs(true);
    let id = builder.unit(unit)?;
    for module in &imported {
        builder.unit(module)?;
    }
    let mut bytes = Vec::new();
    builder.check().typelib(id, &mut bytes)?;
    Ok(bytes)
}

/// A typelib's content as written
fn content(bytes: &[u8]) -> wire::Content<&str> {
    let (_, rest): (Header, _) = postcard::take_from_bytes(bytes).unwrap();
    postcard::from_bytes(rest).unwrap()
}

/// Read a typelib's harvest.
fn read(bytes: &[u8]) -> Result<Harvest<'_>, Invalid> {
    super::read(bytes).map(|(harvest, _)| harvest)
}

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
  @let Local = Int
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
    let unit = config.unit(Path::new("lib.dol"), source);
    let diags: Vec<_> = unit
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .collect();
    assert!(!unit.failed, "the fixture doesn't compile: {diags:?}");
    unit
}

fn typelib() -> Vec<u8> {
    write(&compile(MODULE, Mode::Module { name: "lib".into() }), &[]).unwrap()
}

/// Check the script against a typelib, returning the script's diagnostics.
fn check(typelib: &[u8]) -> Result<Vec<String>, crate::Error> {
    let script = compile(SCRIPT, Mode::Script);
    let typelib = Typelib::read(typelib)?;
    let mut builder = Builder::new();
    builder.typelib(&typelib)?;
    let script = builder.unit(&script).unwrap();
    let check = builder.check();
    check.smoke();
    Ok(check
        .diagnostics()
        .filter(|diag| diag.span().unit() == Some(script))
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
fn text_is_interned_once() {
    // Constants spell the module's own name and path, and a name it declares
    let source = "
pub @let Tag = (\"Tag\" | \"strings\" | \"lib.dol\" | \"tab\\there\")
pub def tag t @ Tag -> Tag
  t
pub let limit @ Tag = \"Tag\"
";
    let unit = compile(
        source,
        Mode::Module {
            name: "strings".into(),
        },
    );
    let bytes = write(&unit, &[]).unwrap();
    let harvest = read(&bytes).unwrap();
    let mut strings: Vec<&str> = harvest.strings.iter().map(|text| &**text).collect();
    strings.sort_unstable();
    assert!(
        strings.windows(2).all(|pair| pair[0] != pair[1]),
        "a string repeats: {strings:?}"
    );
    assert!(strings.contains(&"tab\there"));
    // Each is written once, in the string table
    for text in ["Tag", "strings", "lib.dol", "limit"] {
        let count = bytes
            .windows(text.len())
            .filter(|window| *window == text.as_bytes())
            .count();
        assert_eq!(count, 1, "`{text}` is written {count} times");
    }
}

#[test]
fn types_are_interned_once() {
    let source = "pub def f a @ Array[Int] b @ Array[Int] c @ Str -> Str\n  c\n";
    let bytes = write(&compile(source, Mode::Module { name: "lib".into() }), &[]).unwrap();
    // `Array`, `Int`, `Array[Int]` and `Str`, each once
    assert_eq!(content(&bytes).types.len(), 4);
    let harvest = read(&bytes).unwrap();
    assert_eq!(harvest.sites.len(), 4);
    assert_eq!(encode(harvest), bytes);
}

#[test]
fn type_before_its_children() {
    let bytes = typelib();
    let mut content = content(&bytes);
    content.types.reverse();
    assert!(matches!(
        wire::decode(content),
        Err("a type refers to one after it")
    ));
}

/// The members of a class, each method by its declaration and signature
fn members(harvest: &Harvest<'_>, class: usize) -> Vec<Option<(usize, usize)>> {
    let DeclNode::Class(class) = &harvest.decls[class].node else {
        panic!("the declaration is a class");
    };
    class
        .members
        .iter()
        .map(|member| match *member {
            Member::Field(_) => None,
            Member::Method { decl, sig } => Some((decl.index(), sig)),
        })
        .collect()
}

#[test]
fn methods_keep_their_place_among_fields() {
    let source = "
pub class C
  pub field a @ Int = 0

  pub def m self -> Int
    1

  pub field b @ Int = 0
";
    let bytes = write(&compile(source, Mode::Module { name: "lib".into() }), &[]).unwrap();
    let harvest = read(&bytes).unwrap();
    assert_eq!(members(&harvest, 0), [None, Some((1, 0)), None]);
    assert_eq!(encode(harvest), bytes);
}

/// What a typelib leaves to be derived is written where it can't be.
#[test]
fn underived_values_are_written() {
    let bytes = typelib();
    let mut harvest = read(&bytes).unwrap();
    let elsewhere = Span { start: 0, end: 1 };
    let (double, _) = (harvest.decls.iter().enumerate())
        .find(|(_, decl)| matches!(decl.node, DeclNode::Defs(_)))
        .expect("the module declares a function");
    let DeclNode::Defs(defs) = &mut harvest.decls[double].node else {
        unreachable!()
    };
    defs[0].name.span = elsewhere;
    let def = defs[0].name;
    // `double` has no binders for the site to name
    let site = (harvest.sites.iter())
        .position(|site| site.owner.is_some_and(|(decl, _)| decl.index() == double))
        .expect("a site is in `double`'s signature");
    harvest.sites[site].owner = None;
    harvest.sites[site].ambient = None;
    let mut key = None;
    for decl in &mut harvest.decls {
        let DeclNode::Defs(defs) = &mut decl.node else {
            continue;
        };
        for param in defs.iter_mut().flat_map(|def| &mut def.sig.params) {
            if let ParamKind::Key { key: written } = &mut param.kind {
                written.span = elsewhere;
                key = Some(*written);
            }
        }
    }
    let key = key.expect("a function takes a key");
    let class = (harvest.decls.iter())
        .position(|decl| matches!(decl.node, DeclNode::Class(_)))
        .expect("the module declares a class");
    let DeclNode::Class(written) = &mut harvest.decls[class].node else {
        unreachable!()
    };
    written.members.reverse();
    let order = members(&harvest, class);

    let bytes = encode(harvest);
    let harvest = read(&bytes).unwrap();
    let DeclNode::Defs(defs) = &harvest.decls[double].node else {
        panic!("`double` is a function");
    };
    assert_eq!(defs[0].name, def);
    assert_eq!(harvest.sites[site].owner, None);
    assert_eq!(harvest.sites[site].ambient, None);
    let keys: Vec<_> = (harvest.decls.iter())
        .filter_map(|decl| match &decl.node {
            DeclNode::Defs(defs) => Some(defs),
            _ => None,
        })
        .flatten()
        .flat_map(|def| &def.sig.params)
        .filter_map(|param| match param.kind {
            ParamKind::Key { key } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, [key]);
    assert_eq!(members(&harvest, class), order);
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
    let expected: Vec<_> = from_source
        .diagnostics()
        .filter(script_unit)
        .map(|diag| diag.message().to_string())
        .collect();
    assert_eq!(check(&typelib()).unwrap(), expected);
}

#[test]
fn script_has_no_typelib() {
    let error = write(&compile(SCRIPT, Mode::Script), &["lib"]).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NotModule));
}

#[test]
fn not_requested() {
    let module = compile(MODULE, Mode::Module { name: "lib".into() });
    let mut builder = Builder::new();
    let id = builder.unit(&module).unwrap();
    let error = builder.check().typelib(id, &mut Vec::new()).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NotRequested));
}

#[test]
fn checked_from_typelib() {
    let bytes = typelib();
    let typelib = Typelib::read(&bytes).unwrap();
    let mut builder = Builder::new();
    builder.typelibs(true);
    let id = builder.typelib(&typelib).unwrap();
    let error = builder.check().typelib(id, &mut Vec::new()).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NoSource));
}

#[test]
fn missing_import() {
    let lib = compile(MISSING, Mode::Module { name: "lib".into() });
    let error = write(&lib, &[]).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::MissingImport));
    assert_eq!(error.to_string(), "imported module `gone` was not checked");
}

/// The typelib is the same whatever else the check holds
#[test]
fn written_alone() {
    let lib = compile(
        "import dep\npub @let G = dep.Thing\n",
        Mode::Module { name: "lib".into() },
    );
    let dep = compile("pub @class Thing\n", Mode::Module { name: "dep".into() });
    let script = compile(SCRIPT, Mode::Script);
    let mut builder = Builder::new();
    builder.typelibs(true);
    builder.unit(&script).unwrap();
    builder.unit(&dep).unwrap();
    let std = compile("pub @class Int\n", Mode::Module { name: "std".into() });
    builder.unit(&std).unwrap();
    let id = builder.unit(&lib).unwrap();
    let mut bytes = Vec::new();
    builder.check().typelib(id, &mut bytes).unwrap();
    assert_eq!(bytes, write(&lib, &["dep"]).unwrap());
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
    let bytes = write(
        &compile(source, Mode::Module { name: "lib".into() }),
        &["geometry", "json", "time", "uuid"],
    )
    .unwrap();
    let typelib = Typelib::read(&bytes).unwrap();
    assert_eq!(typelib.module(), "lib");
    assert_eq!(typelib.path(), Path::new("lib.dol"));
    // A module named only in a body is still imported
    assert_eq!(typelib.imports(), ["geometry", "json", "time", "uuid"]);
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
        let export = harvest
            .exports
            .iter()
            .find(|export| harvest.strings[export.name.index()] == name)
            .expect("the name is exported");
        let Target::Local(Referent::Value(value)) = export.target else {
            panic!("`{name}` is exported as a variable");
        };
        harvest
            .values
            .iter()
            .find(|listed| listed.span == value.span)
            .is_some_and(|listed| listed.annot.is_some())
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
    stray.values.insert(
        0,
        Value {
            span: Span::default(),
            annot: None,
        },
    );
    assert!(matches!(
        read(&encode(stray)),
        Err(Invalid::Malformed("a listed variable is not exported"))
    ));
}

#[test]
fn private_items() {
    let source = "
import std:
  - @Phantom

class Hidden

@let Inner = Int

pub class Open[T]
  field marker @ Phantom[T]
  field secret @ Hidden = ...
  pub field shown @ Int = 0

  def (init) self
    nil

  def helper self -> Hidden
    ...

  pub def get self -> Int
    ...

def private_def()
  ...
";
    let bytes = write(
        &compile(source, Mode::Module { name: "lib".into() }),
        &["std"],
    )
    .unwrap();
    let harvest = read(&bytes).unwrap();
    let text = |name: Option<Name>| &*harvest.strings[name.unwrap().text.index()];
    let decls: Vec<_> = harvest.decls.iter().map(|decl| text(decl.name)).collect();
    // The special `(init)` stays whatever its visibility
    assert_eq!(decls, ["Open", "init", "get"]);
    let DeclNode::Class(open) = &harvest.decls[0].node else {
        panic!("`Open` is a class");
    };
    let fields: Vec<_> = open
        .members
        .iter()
        .filter_map(|member| match member {
            Member::Field(field) => Some(text(field.names.first().copied())),
            Member::Method { .. } => None,
        })
        .collect();
    // A private `Phantom` field counts towards the class's variance
    assert_eq!(fields, ["marker", "shown"]);
    assert_eq!(encode(harvest), bytes);
}

#[test]
fn private_in_interface() {
    for private in [
        "class Private",
        "@class Private",
        "@let Private = ...",
        "@let Private = Int",
    ] {
        let source = format!("{private}\n\npub def f x @ Private\n  x\n");
        let unit = compile(&source, Mode::Module { name: "lib".into() });
        let error = write(&unit, &[]).unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::NotExportable),
            "`{private}` is not exportable"
        );
    }
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

const MISSING: &str = "import gone
pub @let G = gone.Thing
";

/// A script that checks only if `G` is unknown
const RECOVERING: &str = "import lib:
  - @G
let _g @ G = 1
";

/// Check the units together, returning each diagnostic with the index of the unit
/// it is in, and its line and column.
fn located(units: &[&Unit<'_>], typelibs: &[&[u8]]) -> Vec<(usize, u32, u32, String)> {
    let typelibs: Vec<_> = typelibs
        .iter()
        .map(|bytes| Typelib::read(bytes).unwrap())
        .collect();
    let mut builder = Builder::new();
    let mut ids = Vec::new();
    for typelib in &typelibs {
        ids.push(builder.typelib(typelib).unwrap());
    }
    for unit in units {
        ids.push(builder.unit(unit).unwrap());
    }
    let check = builder.check();
    check.smoke();
    check
        .diagnostics()
        .map(|diag| {
            let unit = ids.iter().position(|&id| diag.span().unit() == Some(id));
            let start = diag.span().span().start();
            (
                unit.expect("checker diagnostics name their unit"),
                start.line_number(),
                start.column_number(),
                diag.message().to_string(),
            )
        })
        .collect()
}

#[test]
fn missing_module() {
    let lib = compile(MISSING, Mode::Module { name: "lib".into() });
    let script = compile(RECOVERING, Mode::Script);
    // At the import, from source or typelib; the script recovers
    let expected = [(0, 1, 8, "module `gone` not found".to_owned())];
    assert_eq!(located(&[&lib, &script], &[]), expected);
    let bytes = write(&lib, &["gone"]).unwrap();
    assert_eq!(located(&[&script], &[&bytes]), expected);
}

#[test]
fn missing_export_of_dependency() {
    let lib = compile(
        "import dep\npub @let G = dep.Thing\npub @let H = dep.Thing\n",
        Mode::Module { name: "lib".into() },
    );
    let bytes = write(&lib, &["dep"]).unwrap();
    // The dependency no longer exports what the typelib was written against
    let dep = compile("pub @class Other\n", Mode::Module { name: "dep".into() });
    let script = compile(RECOVERING, Mode::Script);
    // Once for the names sharing a link: from source at the first, and from the
    // typelib, which keeps no reference's span, at the import. The script recovers.
    let message = "module `dep` has no export `Thing`".to_owned();
    assert_eq!(
        located(&[&dep, &lib, &script], &[]),
        [(1, 2, 14, message.clone())]
    );
    assert_eq!(located(&[&dep, &script], &[&bytes]), [(0, 1, 8, message)]);
}

#[test]
fn omits_reference_spans_and_spellings() {
    // `Alias` only spells a reference, and `Thing` is the item it imports
    let source = "import dep:\n  @Thing: Alias\npub @let G = Alias\npub @let H = Array[G]\n";
    let unit = compile(source, Mode::Module { name: "lib".into() });
    let bytes = write(&unit, &["dep"]).unwrap();
    let harvest = read(&bytes).unwrap();
    let strings: Vec<&str> = harvest.strings.iter().map(|text| &**text).collect();
    assert!(strings.contains(&"Thing"));
    assert!(
        !strings.contains(&"Alias"),
        "a spelling is kept: {strings:?}"
    );
    let mut names = 0;
    for site in &harvest.sites {
        site.ty.each_node(&mut |ty| {
            assert_eq!(ty.span(), Span::INVALID);
            if let TypeExpr::Name(head) = ty {
                assert!(head.link.is_some());
                names += 1;
            }
        });
    }
    assert_eq!(names, 3);
    assert!(harvest.links.iter().all(|link| link.span == Span::INVALID));
}

#[test]
fn locates_kept_spans() {
    let unit = compile(MODULE, Mode::Module { name: "lib".into() });
    let bytes = write(&unit, &[]).unwrap();
    let harvest = read(&bytes).unwrap();
    // Declarations, binders, members and parameters, among others
    let spans = kept_spans(&harvest);
    assert!(spans.len() > harvest.decls.len());
    for span in spans {
        for offset in [span.start, span.end] {
            assert_eq!(
                harvest.info.coord(offset),
                unit.file.coord(offset),
                "{span:?} is located"
            );
        }
    }
}

#[test]
fn import_on_later_line() {
    let lib = compile(
        "# A dependency\n\nimport dep\npub @let G = dep.Thing\n",
        Mode::Module { name: "lib".into() },
    );
    let bytes = write(&lib, &["dep"]).unwrap();
    let dep = compile("pub @class Other\n", Mode::Module { name: "dep".into() });
    let script = compile(RECOVERING, Mode::Script);
    let message = "module `dep` has no export `Thing`".to_owned();
    assert_eq!(located(&[&dep, &script], &[&bytes]), [(0, 3, 8, message)]);
}

#[test]
fn omitted_span_at_first_line() {
    let lib = compile(
        "import dep\n\npub @let G = dep.Thing[Int]\n",
        Mode::Module { name: "lib".into() },
    );
    let bytes = write(&lib, &["dep"]).unwrap();
    // The dependency's class no longer takes the argument the typelib applies
    let dep = compile("pub @class Thing\n", Mode::Module { name: "dep".into() });
    let script = compile(RECOVERING, Mode::Script);
    let found = located(&[&dep, &script], &[&bytes]);
    assert!(!found.is_empty());
    for (unit, line, column, message) in found {
        assert_eq!((unit, line, column), (0, 1, 1), "{message}");
    }
}
