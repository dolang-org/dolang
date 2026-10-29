use std::{
    collections::BTreeMap,
    panic::{self, AssertUnwindSafe},
    path::Path,
};

use bitvec::slice::BitSlice;

use super::{HEADER, Header, Invalid, wire};
use crate::{
    Config, ErrorKind, Mode, Unit,
    diag::Span,
    typeck::{
        Builder, Typelib, UnitId,
        elab::{
            Bases, BinderRef, DeclNode, Facts, Referent, Shard, Tables, Target, Value,
            surface::{Member, Name, ParamKind, StrId, TypeExpr},
        },
        report::resolve_span,
        r#type::Variance,
    },
};

/// Encode the surface of a shard that is not yet linked, with its facts and the
/// pipe nominations it records.
fn encode(shard: Shard<'_>, facts: &Facts) -> Vec<u8> {
    let (kept, _) = shard.interface();
    let text = |id: StrId| &*shard.strings[id.index()];
    let pipes =
        (shard.nominations).map(|pipes| pipes.map(|(module, item)| (text(module), text(item))));
    super::write(&shard, &Bases::local(), &kept, facts, pipes)
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

/// The bytes of a typelib's content
fn serialize(content: &wire::Content<&str>) -> Vec<u8> {
    let out = postcard::to_stdvec(&HEADER).unwrap();
    postcard::to_extend(content, out).unwrap()
}

/// Read a typelib's shard and the facts it records.
fn read(bytes: &[u8]) -> Result<(Shard<'_>, Facts), Invalid> {
    super::read(bytes)
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
    let (shard, facts) = read(&bytes).unwrap();
    assert!(
        shard.decls.iter().all(|decl| decl.name.is_some()),
        "the bodies' declarations are left out"
    );
    assert_eq!(encode(shard, &facts), bytes);
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
    let (shard, _) = read(&bytes).unwrap();
    let mut strings: Vec<&str> = shard.strings.iter().map(|text| &**text).collect();
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
    let (shard, facts) = read(&bytes).unwrap();
    assert_eq!(shard.sites.len(), 4);
    assert_eq!(encode(shard, &facts), bytes);
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
fn members(shard: &Shard<'_>, class: usize) -> Vec<Option<(usize, usize)>> {
    let DeclNode::Class(class) = &shard.decls[class].node else {
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
    let (shard, facts) = read(&bytes).unwrap();
    assert_eq!(members(&shard, 0), [None, Some((1, 0)), None]);
    assert_eq!(encode(shard, &facts), bytes);
}

/// What a typelib leaves to be derived is written where it can't be.
#[test]
fn underived_values_are_written() {
    let bytes = typelib();
    let (mut shard, facts) = read(&bytes).unwrap();
    let elsewhere = Span::new(0, 0, 1);
    let (double, _) = (shard.decls.iter().enumerate())
        .find(|(_, decl)| matches!(decl.node, DeclNode::Defs(_)))
        .expect("the module declares a function");
    let DeclNode::Defs(defs) = &mut shard.decls[double].node else {
        unreachable!()
    };
    defs[0].name.span = elsewhere;
    // Strings are renumbered as they are met, so names are compared by text
    let spelled =
        |shard: &Shard<'_>, name: Name| (name.span, shard.strings[name.text.index()].to_string());
    let def = defs[0].name;
    let def = spelled(&shard, def);
    // `double` has no binders for the site to name
    let site = (shard.sites.iter())
        .position(|site| site.owner.is_some_and(|(decl, _)| decl.index() == double))
        .expect("a site is in `double`'s signature");
    shard.sites[site].owner = None;
    shard.sites[site].ambient = None;
    let mut key = None;
    for decl in &mut shard.decls {
        let DeclNode::Defs(defs) = &mut decl.node else {
            continue;
        };
        for param in defs.iter_mut().flat_map(|def| &mut def.sig.params) {
            if let ParamKind::Key { key: written } = &mut param.kind {
                written.span = elsewhere;
                key = Some((written.span, written.text));
            }
        }
    }
    let (span, text) = key.expect("a function takes a key");
    let key = (span, shard.strings[text.index()].to_string());
    let class = (shard.decls.iter())
        .position(|decl| matches!(decl.node, DeclNode::Class(_)))
        .expect("the module declares a class");
    let DeclNode::Class(written) = &mut shard.decls[class].node else {
        unreachable!()
    };
    written.members.reverse();
    let order = members(&shard, class);

    let bytes = encode(shard, &facts);
    let (shard, facts) = read(&bytes).unwrap();
    let DeclNode::Defs(defs) = &shard.decls[double].node else {
        panic!("`double` is a function");
    };
    assert_eq!(spelled(&shard, defs[0].name), def);
    assert_eq!(shard.sites[site].owner, None);
    assert_eq!(shard.sites[site].ambient, None);
    let keys: Vec<_> = (shard.decls.iter())
        .filter_map(|decl| match &decl.node {
            DeclNode::Defs(defs) => Some(defs),
            _ => None,
        })
        .flatten()
        .flat_map(|def| &def.sig.params)
        .filter_map(|param| match param.kind {
            ParamKind::Key { key } => Some(spelled(&shard, key)),
            _ => None,
        })
        .collect();
    assert_eq!(keys, [key]);
    assert_eq!(members(&shard, class), order);
    assert_eq!(encode(shard, &facts), bytes);
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
    let mut content = content(&bytes);
    // The last string is the last one met, which the module's name and path aren't
    content.strings.pop();
    assert!(matches!(
        read(&serialize(&content)),
        Err(Invalid::Malformed("an ID is out of range"))
    ));
}

#[test]
fn malformed_facts() {
    let bytes = typelib();
    // `Box`, whose one signature has one binder
    let mut corrupt = content(&bytes);
    corrupt.facts[0].variance[0].pop();
    assert!(matches!(
        read(&serialize(&corrupt)),
        Err(Invalid::Malformed("the variances are not one per binder"))
    ));
    let mut corrupt = content(&bytes);
    corrupt.facts[0].variance.push(Vec::new());
    assert!(matches!(
        read(&serialize(&corrupt)),
        Err(Invalid::Malformed(
            "the variances are not one list per signature"
        ))
    ));
    // `Box` is outermost, so it captures nothing
    let mut corrupt = content(&bytes);
    corrupt.facts[0]
        .captured
        .push(((0, 0, 0), wire::WireVariance::Covariant));
    assert!(matches!(
        read(&serialize(&corrupt)),
        Err(Invalid::Malformed(
            "a captured binder is not of an outer signature"
        ))
    ));
    let mut corrupt = content(&bytes);
    corrupt.facts.pop();
    assert!(matches!(
        read(&serialize(&corrupt)),
        Err(Invalid::Malformed("the facts are not one per declaration"))
    ));
}

#[test]
fn exported_variables() {
    let bytes = typelib();
    let (shard, _) = read(&bytes).unwrap();
    let annotated = |name: &str| {
        let export = shard
            .exports
            .iter()
            .find(|export| shard.strings[export.name.index()] == name)
            .expect("the name is exported");
        let Target::Local(Referent::Value(value)) = export.target else {
            panic!("`{name}` is exported as a variable");
        };
        shard
            .values
            .iter()
            .find(|listed| listed.span == value.span)
            .is_some_and(|listed| listed.annot.is_some())
    };
    assert!(annotated("limit"));
    assert!(!annotated("loose"));
    assert_eq!(shard.values.len(), 2);

    let (mut unlisted, facts) = read(&bytes).unwrap();
    unlisted.values.clear();
    assert!(matches!(
        read(&encode(unlisted, &facts)),
        Err(Invalid::Malformed("an exported variable is not listed"))
    ));

    let (mut stray, facts) = read(&bytes).unwrap();
    stray.values.insert(
        0,
        Value {
            span: Span::new(0, 0, 0),
            annot: None,
        },
    );
    assert!(matches!(
        read(&encode(stray, &facts)),
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
    let (shard, facts) = read(&bytes).unwrap();
    let text = |name: Option<Name>| &*shard.strings[name.unwrap().text.index()];
    let decls: Vec<_> = shard.decls.iter().map(|decl| text(decl.name)).collect();
    // The special `(init)` stays whatever its visibility
    assert_eq!(decls, ["Open", "init", "get"]);
    let DeclNode::Class(open) = &shard.decls[0].node else {
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
    // What a private field decides, the class's variance, is recorded instead
    assert_eq!(fields, ["shown"]);
    assert_eq!(encode(shard, &facts), bytes);
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
    let (shard, _) = read(&bytes).unwrap();
    let strings: Vec<&str> = shard.strings.iter().map(|text| &**text).collect();
    assert!(strings.contains(&"Thing"));
    assert!(
        !strings.contains(&"Alias"),
        "a spelling is kept: {strings:?}"
    );
    let mut names = 0;
    for site in &shard.sites {
        site.ty.each_node(&mut |ty| {
            assert_eq!(ty.span(), Span::INVALID);
            if let TypeExpr::Name(head) = ty {
                assert!(head.link.is_some());
                names += 1;
            }
        });
    }
    assert_eq!(names, 3);
    assert!(shard.links.iter().all(|link| link.span == Span::INVALID));
}

#[test]
fn locates_kept_spans() {
    // Kept spans after non-ASCII text and a tab on their line, and on later lines
    let source = "pub def f :key @ Str = \"é\tü\" *rest @ Int -> Int\n  1\n\
                  pub @let A[T] = Int # ü\npub @let B = Int\n";
    let unit = compile(source, Mode::Module { name: "lib".into() });
    let std = compile("", Mode::Module { name: "std".into() });
    let mut builder = Builder::new();
    builder.typelibs(true);
    let id = builder.unit(&unit).unwrap();
    builder.unit(&std).unwrap();
    let (check, tables) = builder.check_tables();
    let mut bytes = Vec::new();
    check.typelib(id, &mut bytes).unwrap();
    let located = |shard: &Shard<'_>, base: &Bases, kept: &BitSlice, facts: &Facts| {
        let (_, spans) = wire::Encode::new(shard, base, kept, facts, None).content();
        (spans.into_iter())
            .filter(|&span| span != Span::INVALID)
            .map(|span| resolve_span(id, span).span())
            .collect::<Vec<_>>()
    };
    let shard = &tables.shards[id.index()];
    let kept = shard.kept.as_ref().unwrap();
    let from_source = located(shard, tables.base(id), kept, &tables.facts[id.index()]);
    let (shard, facts) = read(&bytes).unwrap();
    let (kept, _) = shard.interface();
    let from_typelib = located(&shard, &Bases::local(), &kept, &facts);
    // Declarations, binders, members and parameters, among others
    assert!(from_source.len() > shard.decls.len());
    assert_eq!(from_typelib, from_source);
    let rest = from_source
        .iter()
        .find(|span| span.start().column_offset() == 30);
    assert_eq!(rest.map(|span| span.len()), Some(4), "{from_source:?}");
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

/// A `std` declaring `Phantom`, which variance counts
const PHANTOM_STD: &str = "pub @let Phantom[T] = ...\npub @class Int\n";

const VARIANT: &str = "
import std:
  - @Phantom

pub class Out[T]
  field marker @ Phantom[T]

pub class In[T]
  field sink @ T = ...

  pub def put self x @ T
    nil

pub class Hidden[T]
  field kept @ T = ...

  def helper self -> T
    ...

pub class Outer[T]
  pub def get self -> T
    ...

pub @let Fn[A, B] = (A -> B)

pub def apply[T] f @ (T -> nil)
  nil
";

/// The variance of each binder of a unit's declarations, and of each outer binder
/// they use, by the declaration's name, signature and slot
fn variances(
    tables: &Tables<'_>,
    unit: UnitId,
) -> BTreeMap<(String, usize, usize, bool), Variance> {
    let name = |decl| {
        let text = tables.decl(decl).name.map(|name| name.text);
        text.map_or("", |text| &*tables.shard(unit).strings[text.index()])
    };
    let mut found = BTreeMap::new();
    for decl in tables.unit_decl_ids(unit) {
        for sig in 0..tables.sig_count(decl) {
            let mut slot = 0;
            while let Some(variance) = tables.variance(BinderRef { decl, sig, slot }) {
                found.insert((name(decl).to_owned(), sig, slot, false), variance);
                slot += 1;
            }
        }
    }
    for (&(decl, binder), &variance) in &tables.facts[unit.index()].captured {
        let key = (
            format!("{}.{}", name(binder.decl), name(decl)),
            binder.sig,
            binder.slot,
            true,
        );
        found.insert(key, variance);
    }
    found
}

#[test]
fn variance_is_recorded() {
    let std = compile(PHANTOM_STD, Mode::Module { name: "std".into() });
    let lib = compile(VARIANT, Mode::Module { name: "lib".into() });
    let mut builder = Builder::new();
    builder.typelibs(true);
    builder.unit(&std).unwrap();
    let id = builder.unit(&lib).unwrap();
    let (check, tables) = builder.check_tables();
    let mut bytes = Vec::new();
    check.typelib(id, &mut bytes).unwrap();
    let mut from_source = variances(&tables, id);

    let typelib = Typelib::read(&bytes).unwrap();
    let mut builder = Builder::new();
    builder.unit(&std).unwrap();
    let id = builder.typelib(&typelib).unwrap();
    let (_, tables) = builder.check_tables();
    let from_typelib = variances(&tables, id);

    // The private method isn't on the surface
    from_source.retain(|(name, ..), _| !name.ends_with("helper"));
    assert_eq!(from_typelib, from_source);
    let of = |name: &str, slot| from_typelib[&(name.to_owned(), 0, slot, false)];
    assert_eq!(of("Out", 0), Variance::Covariant);
    assert_eq!(of("In", 0), Variance::Contravariant);
    assert_eq!(of("Hidden", 0), Variance::Invariant);
    assert_eq!(of("Fn", 0), Variance::Contravariant);
    assert_eq!(of("Fn", 1), Variance::Covariant);
    assert_eq!(
        from_typelib[&("Outer.get".to_owned(), 0, 0, true)],
        Variance::Covariant
    );
}

const STRAND: &str = "pub @let PipeSender[I] = ...
pub @let PipeReceiver[I] = ...
";

const PROC: &str = "pub class Sender[T]
  pub def send self x @ T
    nil
pub class Receiver[T]
  pub def recv self -> T
    ...
";

const PIPES: [(&str, &str); 2] = [("proc", "Sender"), ("proc", "Receiver")];

/// Write the typelibs of `strand`, `proc` and modules depending on them or not,
/// with `PIPES` nominated
fn piped() -> [Vec<u8>; 4] {
    let units = [
        compile(
            STRAND,
            Mode::Module {
                name: "strand".into(),
            },
        ),
        compile(
            PROC,
            Mode::Module {
                name: "proc".into(),
            },
        ),
        compile(
            "import strand\npub @let P = strand.PipeSender[Int]\n",
            Mode::Module { name: "dep".into() },
        ),
        compile(
            "pub @let Q = Int\n",
            Mode::Module {
                name: "other".into(),
            },
        ),
    ];
    let std = compile("pub @class Int\n", Mode::Module { name: "std".into() });
    let mut builder = Builder::new();
    builder.typelibs(true).pipes(PIPES[0], PIPES[1]);
    builder.unit(&std).unwrap();
    let ids: Vec<_> = units
        .iter()
        .map(|unit| builder.unit(unit).unwrap())
        .collect();
    let check = builder.check();
    let typelibs: Vec<_> = (ids.into_iter())
        .map(|id| {
            let mut bytes = Vec::new();
            check.typelib(id, &mut bytes).unwrap();
            bytes
        })
        .collect();
    typelibs.try_into().unwrap()
}

#[test]
fn records_pipes() {
    let [strand, proc, dep, other] = piped();
    let nominations = |bytes: &[u8]| {
        let (shard, _) = read(bytes).unwrap();
        let text = |id: StrId| shard.strings[id.index()].to_string();
        (shard.nominations).map(|pipes| pipes.map(|(module, item)| (text(module), text(item))))
    };
    let expected = Some(PIPES.map(|(module, item)| (module.to_owned(), item.to_owned())));
    assert_eq!(nominations(&strand), expected);
    assert_eq!(nominations(&dep), expected);
    assert_eq!(nominations(&proc), None);
    assert_eq!(nominations(&other), None);
}

#[test]
fn rejects_other_pipes() {
    let [strand, ..] = piped();
    let typelib = Typelib::read(&strand).unwrap();
    let error = Builder::new().typelib(&typelib).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::Pipes));
    assert_eq!(
        error.to_string(),
        "typelib of `strand` was written with other pipe types"
    );
    let mut builder = Builder::new();
    builder.pipes(PIPES[0], PIPES[1]);
    builder.typelib(&typelib).unwrap();
}

#[test]
#[should_panic = "pipes are nominated before typelibs are added"]
fn pipes_before_typelibs() {
    let bytes = typelib();
    let typelib = Typelib::read(&bytes).unwrap();
    let mut builder = Builder::new();
    builder.typelib(&typelib).unwrap();
    builder.pipes(PIPES[0], PIPES[1]);
}

#[test]
fn recorded_nominee_not_found() {
    let [strand, ..] = piped();
    let typelib = Typelib::read(&strand).unwrap();
    let std = compile("pub @class Int\n", Mode::Module { name: "std".into() });
    let mut builder = Builder::new();
    builder.pipes(PIPES[0], PIPES[1]);
    builder.unit(&std).unwrap();
    builder.typelib(&typelib).unwrap();
    // The minimal `std` misses what the checker requires, which it reports too
    let messages: Vec<_> = (builder.check().diagnostics())
        .map(|diag| diag.message().to_string())
        .filter(|message| message.ends_with("not found"))
        .collect();
    assert_eq!(
        messages,
        ["module `proc` not found", "module `proc` not found"]
    );
}

#[test]
fn missing_nominee() {
    let strand = compile(
        STRAND,
        Mode::Module {
            name: "strand".into(),
        },
    );
    let other = compile(
        "pub @let Q = Int\n",
        Mode::Module {
            name: "other".into(),
        },
    );
    let std = compile("pub @class Int\n", Mode::Module { name: "std".into() });
    let mut builder = Builder::new();
    builder.typelibs(true).pipes(PIPES[0], PIPES[1]);
    builder.unit(&std).unwrap();
    let ids = [
        builder.unit(&strand).unwrap(),
        builder.unit(&other).unwrap(),
    ];
    let check = builder.check();
    let error = check.typelib(ids[0], &mut Vec::new()).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::MissingImport));
    assert_eq!(
        error.to_string(),
        "module `proc` of a nominated pipe type was not checked"
    );
    // What doesn't depend on the pipes is written
    check.typelib(ids[1], &mut Vec::new()).unwrap();
}
