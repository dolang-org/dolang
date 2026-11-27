//! What hover shows at a position, as data for the page to render.
//!
//! The playground's one unit is its own module, and every other is a bundled
//! typelib, so a declaration found in one is left as a key into the doc index
//! the page loads. Signatures are returned in parts, since how one is laid out
//! is up to the page.
use dolang::compile::{Unit, UnitId, typeck};
use dolang_ide::{Declaration, Reference, TypeLine};
use dolang_util::utf16::Offsets;
use serde::Serialize;

/// What a check concluded about the unit
pub(crate) struct Checked {
    pub(crate) unit: UnitId,
    /// The unit's module name in the check
    pub(crate) name: String,
    pub(crate) analysis: typeck::Analysis,
}

impl Checked {
    /// The check as `dolang_ide` takes it, `unit` being the source's unit,
    /// the only module with one
    pub(crate) fn ide<'a>(
        &'a self,
        unit: &'a Unit<'static>,
    ) -> dolang_ide::Checked<'a, impl Fn(&str) -> Option<(&'a Unit<'a>, ())>> {
        dolang_ide::Checked {
            analysis: &self.analysis,
            unit: self.unit,
            name: &self.name[..],
            resolve: move |module: &str| (module == self.name).then_some((unit, ())),
        }
    }
}

/// What hover shows at a position. Offsets are UTF-16.
#[derive(Debug, Serialize)]
pub(crate) struct Hover {
    /// The reference under the position, or else the checked node's span
    pub from: usize,
    pub to: usize,
    /// What the check concluded there, empty before a check
    pub types: Vec<TypeLine>,
    /// The declarations named there
    pub declarations: Vec<Declaration>,
    /// Whether every declaration in the source found by the check was
    /// described. A bundled key the page has no docs for isn't either.
    pub complete: bool,
}

/// A token naming a declaration, by UTF-16 offsets
#[derive(Debug, Serialize)]
pub(crate) struct Occurrence {
    pub from: usize,
    pub to: usize,
    /// Whether it's the name the declaration binds, as a `let` or a
    /// parameter does, which is the only write one unit can be sure of
    pub write: bool,
}

/// Every token naming the declaration that the token under the UTF-16
/// offset `pos` names, its own name included, in source order. Nothing there
/// names a declaration if it's empty.
pub(crate) fn occurrences(
    unit: &Unit<'_>,
    offsets: &Offsets,
    refs: &[Reference],
    pos: usize,
) -> Vec<Occurrence> {
    let start = offsets.byte(pos);
    let at = unit.locate(start..start);
    let Some(id) = dolang_ide::reference_at(unit, refs, at).map(|reference| reference.id) else {
        return Vec::new();
    };
    let definition = unit
        .node(id)
        .and_then(|node| node.definition())
        .map(|span| unit.byte_range(span));
    refs.iter()
        .filter(|reference| reference.id == id)
        .map(|reference| {
            let range = unit.byte_range(reference.span);
            Occurrence {
                from: offsets.utf16(range.start),
                to: offsets.utf16(range.end),
                write: definition == Some(range),
            }
        })
        .collect()
}

/// What hover shows of `unit` at the UTF-16 offsets `from` to `to`: a
/// position when they're equal, else the innermost node containing the range.
pub(crate) fn hover(
    unit: &Unit<'static>,
    offsets: &Offsets,
    refs: &[Reference],
    checked: Option<&Checked>,
    from: usize,
    to: usize,
) -> Option<Hover> {
    let (start, end) = (offsets.byte(from), offsets.byte(to));
    if end < start {
        return None;
    }
    let at = unit.locate(start..end);
    let checked = checked.map(|checked| checked.ide(unit));
    let hover = dolang_ide::hover(unit, refs, checked.as_ref(), at, |declared| vec![declared])?;
    let range = unit.byte_range(hover.span);
    Some(Hover {
        from: offsets.utf16(range.start),
        to: offsets.utf16(range.end),
        types: hover.types,
        declarations: hover.declarations,
        complete: hover.complete,
    })
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use super::*;
    use crate::analysis::Analyzed;
    use dolang_ide::{LineRole, Param, Shape, Signature};
    use wasm_bindgen_test::wasm_bindgen_test;

    /// The UTF-16 offset of the last `needle` in `source`
    fn last(source: &str, needle: &str) -> usize {
        source[..source.rfind(needle).unwrap()]
            .encode_utf16()
            .count()
    }

    fn checked(source: &str) -> Analyzed {
        let mut analyzed = Analyzed::new(source.to_owned());
        analyzed.check();
        analyzed
    }

    fn bundled(hover: &Hover) -> Vec<(&str, &str, Option<u32>)> {
        hover
            .declarations
            .iter()
            .map(|declaration| match declaration {
                Declaration::Bundled {
                    module,
                    item,
                    overload,
                } => (&module[..], &item[..], *overload),
                other => panic!("not bundled: {other:?}"),
            })
            .collect()
    }

    /// The occurrences of the name at `at`, as the start of each, the text
    /// it covers and whether it's a write
    fn occurrences_at(source: &str, at: usize) -> Vec<(usize, String, bool)> {
        let units: Vec<u16> = source.encode_utf16().collect();
        Analyzed::new(source.to_owned())
            .occurrences(at)
            .into_iter()
            .map(|span| {
                let text = String::from_utf16(&units[span.from..span.to]).unwrap();
                (span.from, text, span.write)
            })
            .collect()
    }

    #[wasm_bindgen_test]
    fn occurrences_are_the_names_of_one_declaration() {
        // Characters before them are two and four bytes, and one and two code
        // units
        let source = "# é 😀\nlet x = 1\nlet x = (x + 1)\necho $x\n";
        let x = |at: usize, write| (at, "x".to_owned(), write);
        let second = last(source, "x = (");
        assert_eq!(
            occurrences_at(source, last(source, "x")),
            [x(second, true), x(last(source, "x"), false)]
        );
        let first = source[..source.find("x = 1").unwrap()]
            .encode_utf16()
            .count();
        assert_eq!(
            occurrences_at(source, first),
            [x(first, true), x(last(source, "x + 1"), false)]
        );
    }

    #[wasm_bindgen_test]
    fn a_parameter_is_written_where_it_is_declared() {
        let source = "def double n\n  (n * 2)\n";
        let n = |at: usize, write| (at, "n".to_owned(), write);
        assert_eq!(
            occurrences_at(source, last(source, "n")),
            [n(last(source, "n\n"), true), n(last(source, "n"), false)]
        );
    }

    #[wasm_bindgen_test]
    fn occurrences_of_a_prelude_item_and_of_nothing() {
        // A prelude binding is bound by no name in the source
        let source = "echo a\necho b";
        let echo = |at: usize| (at, "echo".to_owned(), false);
        assert_eq!(
            occurrences_at(source, last(source, "echo")),
            [echo(0), echo(7)]
        );
        assert!(occurrences_at(source, last(source, "b")).is_empty());
    }

    #[wasm_bindgen_test]
    fn variable_read_has_its_type_and_declaration() {
        // Characters before it are two and four bytes, and one and two code
        // units
        let source = "# é 😀\nlet count = 1\necho $count";
        let at = last(source, "count");
        let hover = checked(source).hover(at, at).unwrap();
        assert_eq!((hover.from, hover.to), (at, at + 5));
        let [line] = &hover.types[..] else {
            panic!("{:?}", hover.types)
        };
        assert_eq!(
            (line.role, line.name.as_deref()),
            (LineRole::Variable, Some("count"))
        );
        assert_eq!(line.ty, "1");
        let [
            Declaration::Local {
                shape: Shape::Value,
                signature: Signature::Let { name, .. },
                ..
            },
        ] = &hover.declarations[..]
        else {
            panic!("{:?}", hover.declarations)
        };
        assert_eq!(name, "count");
    }

    #[wasm_bindgen_test]
    fn local_alias_has_its_signature_where_a_type_names_it() {
        // A comment opening the source documents the module instead
        let source = "let w = 0\n# Two of a kind.\npub @let Pair[T] = Tuple[T, T]\n@let Hidden = ...\ndef swap p@(Pair[Hidden])\n  (p[1], p[0])\n@let Service = Dict $\n  \"name\": Str\n  ?\"replicas\": Int\ndef name service @ Service\n  service[\"name\"]\n";
        let analyzed = checked(source);
        let alias = |at: usize| {
            let hover = analyzed.hover(at, at).unwrap();
            let [
                Declaration::Local {
                    doc,
                    signature:
                        Signature::Alias {
                            is_pub,
                            name,
                            binders,
                            ty,
                        },
                    ..
                },
            ] = &hover.declarations[..]
            else {
                panic!("{:?}", hover.declarations)
            };
            (
                doc.clone(),
                *is_pub,
                name.clone(),
                binders.clone(),
                ty.clone(),
            )
        };
        let pair = (
            "Two of a kind.".to_owned(),
            true,
            "Pair".to_owned(),
            vec!["T".to_owned()],
            Some("Tuple[T, T]".to_owned()),
        );
        // Its declaration, and its use in an annotation
        let declared = source.find("Pair").unwrap() + 1;
        assert_eq!(alias(declared), pair);
        assert_eq!(alias(last(source, "Pair") + 1), pair);
        // An opaque alias has no type to write
        let hidden = (String::new(), false, "Hidden".to_owned(), Vec::new(), None);
        assert_eq!(alias(last(source, "Hidden") + 1), hidden);
        // A vertical type keeps its lines
        let (.., ty) = alias(last(source, "Service") + 1);
        let lines = "Dict $\n  \"name\": Str\n  ?\"replicas\": Int";
        assert_eq!(ty.as_deref(), Some(lines));
    }

    #[wasm_bindgen_test]
    fn unchecked_source_has_declarations_only() {
        let source = "let count = 1\necho $count";
        let at = last(source, "count");
        let hover = Analyzed::new(source.to_owned()).hover(at, at).unwrap();
        assert!(hover.types.is_empty());
        assert!(matches!(
            &hover.declarations[..],
            [Declaration::Local {
                signature: Signature::Let { .. },
                ..
            }]
        ));
    }

    #[wasm_bindgen_test]
    fn local_function_call_has_its_signature_and_doc() {
        // A comment opening the source documents the module instead
        let source =
            "let n = 2\n# Doubles a number.\ndef double x@Int -> Int\n  (x * 2)\ndouble $n";
        let at = last(source, "double");
        let hover = checked(source).hover(at, at).unwrap();
        // The callee's name is a read of the function
        let [line] = &hover.types[..] else {
            panic!("{:?}", hover.types)
        };
        // Parenthesized, as an annotation needs it to be
        assert_eq!(line.ty, "((std.Int) -> std.Int)");
        let [
            Declaration::Local {
                shape: Shape::Signature,
                doc,
                signature:
                    Signature::Def {
                        name,
                        params,
                        returns,
                        ..
                    },
                ..
            },
        ] = &hover.declarations[..]
        else {
            panic!("{:?}", hover.declarations)
        };
        assert_eq!(doc, "Doubles a number.");
        assert_eq!(name, "double");
        let [Param { head, ty, default }] = &params[..] else {
            panic!("{params:?}")
        };
        assert_eq!(
            (&head[..], ty.as_deref(), *default),
            ("x", Some("Int"), false)
        );
        assert_eq!(returns.as_deref(), Some("Int"));
    }

    #[wasm_bindgen_test]
    fn local_signature_has_its_input_and_output() {
        let source = "def parse_log x <Iter[Str] >Sink[(Str, Str)]\n  nil\nparse_log 1";
        let at = last(source, "parse_log");
        let hover = Analyzed::new(source.to_owned()).hover(at, at).unwrap();
        let [
            Declaration::Local {
                signature: Signature::Def { params, .. },
                ..
            },
        ] = &hover.declarations[..]
        else {
            panic!("{:?}", hover.declarations)
        };
        let params: Vec<_> = params
            .iter()
            .map(|param| (&param.head[..], param.ty.as_deref(), param.default))
            .collect();
        assert_eq!(
            params,
            [
                ("x", None, false),
                ("<Iter[Str]", None, false),
                (">Sink[(Str, Str)]", None, false)
            ]
        );
    }

    #[wasm_bindgen_test]
    fn overloaded_function_is_its_overloads() {
        // A comment opening the source documents the module instead
        let source = "\
let n = 1
# Twice a number
@def twice x @ Int -> Int
@def twice x @ Str -> Str
# Twice anything
def twice x
  (x + x)
let f = twice";
        let at = last(source, "twice");
        for analyzed in [Analyzed::new(source.to_owned()), checked(source)] {
            let hover = analyzed.hover(at, at).unwrap();
            let described: Vec<_> = hover
                .declarations
                .iter()
                .map(|declaration| match declaration {
                    Declaration::Local {
                        doc,
                        signature:
                            Signature::Def {
                                type_only, returns, ..
                            },
                        ..
                    } => (&doc[..], *type_only, returns.as_deref()),
                    other => panic!("not a local def: {other:?}"),
                })
                .collect();
            // An overload with no doc comment of its own has its
            // implementation's
            assert_eq!(
                described,
                [
                    ("Twice a number", true, Some("Int")),
                    ("Twice anything", true, Some("Str"))
                ]
            );
        }
    }

    #[wasm_bindgen_test]
    fn prelude_call_names_a_bundled_item() {
        let hover = checked("echo hi").hover(1, 1).unwrap();
        assert_eq!(bundled(&hover), [("term", "echo", None)]);
        assert_eq!((hover.from, hover.to), (0, 4));
    }

    #[wasm_bindgen_test]
    fn overloaded_call_names_the_chosen_overload() {
        let source = "int \"ff\" base: 16";
        let hover = checked(source).hover(0, 0).unwrap();
        assert_eq!(bundled(&hover), [("std", "int", Some(1))]);
    }

    #[wasm_bindgen_test]
    fn bundled_method_is_dotted_by_its_class() {
        let source = "let s = \"a\"\necho $s.upper()";
        let at = last(source, "upper");
        let hover = checked(source).hover(at, at).unwrap();
        assert_eq!(bundled(&hover), [("std", "Str.upper", None)]);
    }

    #[wasm_bindgen_test]
    fn type_item_through_a_module_names_a_bundled_item() {
        let source = "import json: j\nimport toml\ndef f() -> std.Never\n  throw std.ValueError \"x\"\ndef g _v@(j.DecodeValue | nil)\n  nil\nlet v = ({a: 1} !@ toml.EncodeValue)\n";
        let analyzed = checked(source);
        // Through the prelude's module, through an import inside a group, and
        // in a cast, which is outside any declaration
        for (needle, module, item) in [
            ("Never", "std", "Never"),
            ("DecodeValue", "json", "DecodeValue"),
            ("EncodeValue", "toml", "EncodeValue"),
        ] {
            let at = last(source, needle);
            let hover = analyzed.hover(at + 2, at + 2).unwrap();
            assert_eq!(bundled(&hover), [(module, item, None)]);
            assert_eq!((hover.from, hover.to), (at, at + needle.len()));
            assert!(hover.types.is_empty(), "{:?}", hover.types);
            // A range that is exactly the item's name
            let hover = analyzed.hover(at, at + needle.len()).unwrap();
            assert_eq!(bundled(&hover), [(module, item, None)]);
        }
    }

    #[wasm_bindgen_test]
    fn nothing_describable_is_none() {
        let source = "let x = 1\n\n# note\n";
        let analyzed = checked(source);
        let blank = last(source, "\n\n") + 1;
        assert!(analyzed.hover(blank, blank).is_none());
        let comment = last(source, "note");
        assert!(analyzed.hover(comment, comment).is_none());
    }

    #[wasm_bindgen_test]
    fn ranges_take_the_innermost_node_containing_them() {
        let source = "let count = (1 + 2)\necho $count";
        let analyzed = checked(source);
        // Exactly a reference, as the position in it is
        let at = last(source, "count");
        let hover = analyzed.hover(at, at + 5).unwrap();
        assert_eq!((hover.from, hover.to), (at, at + 5));
        assert!(!hover.declarations.is_empty());
        // An expression
        let sum = last(source, "1 + 2");
        let hover = analyzed.hover(sum, sum + 5).unwrap();
        assert!(hover.from <= sum && sum + 5 <= hover.to);
        assert!(!hover.types.is_empty());
        assert!(hover.declarations.is_empty());
    }
}
