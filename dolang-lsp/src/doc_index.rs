//! Static per-module documentation index, baked in at build time from the
//! same per-module JSON dump `dodo mkdocs` produces (see `extract_doc_json`
//! in dodo.dol and `build.rs`). Used by hover to describe an identifier that
//! names something outside the file being edited -- an imported or prelude
//! item -- which has no declaration in this document to read a doc comment
//! from.

use crate::signature;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../build.rs"]
mod doc_build;

#[cfg(test)]
pub(crate) use doc_build::typelib_build;

/// One parameter of a documented function or method.
pub(crate) struct Param {
    pub(crate) name: &'static str,
    pub(crate) optional: bool,
    /// The type the parameter is annotated with, as written
    pub(crate) type_: Option<&'static str>,
}

/// One documented module, class, function, value, field, or method.
///
/// `item` is empty for a module's own entry, and dotted (`Class.method`,
/// `Class.(init)`) for a class member -- the same qualified-name convention
/// the mkdocs-autorefs cross-references in docs/ use.
pub(crate) struct DocEntry {
    pub(crate) module: &'static str,
    pub(crate) item: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) doc: &'static str,
    /// Type binders as declared, with any `:` or `...` sigil, bound, and default.
    pub(crate) binders: &'static [&'static str],
    /// A class's `final` and `mixable` decorators, by name
    pub(crate) decorators: &'static [&'static str],
    pub(crate) params: &'static [Param],
    /// The annotated type, as written: a function or method's return type, or a
    /// field's type
    pub(crate) type_: Option<&'static str>,
}

// The real table depends on DOLANG_LSP_DOC_JSON_DIR having been set at build
// time (see build.rs), which is only true for `dodo install` -- an ordinary
// `cargo test` run gets an empty table and could never exercise a lookup.
// Tests get a small fixed fixture instead, independent of that environment.
#[cfg(not(test))]
include!(concat!(env!("OUT_DIR"), "/doc_index_data.rs"));

#[cfg(test)]
pub(crate) static ENTRIES: &[DocEntry] = &[
    DocEntry {
        module: "std",
        item: "Never",
        kind: "alias",
        doc: "The type with no values, which a function that never returns can declare as its result.",
        binders: &[],
        decorators: &[],
        params: &[],
        type_: Some("Union[]"),
    },
    DocEntry {
        module: "term",
        item: "echo",
        kind: "function",
        doc: "Writes to standard output.",
        binders: &[],
        decorators: &[],
        params: &[Param {
            name: "...args",
            optional: false,
            type_: None,
        }],
        type_: Some("nil"),
    },
];

/// Looks up a documented module/item pair: one entry, or one per overload in
/// source order.
///
/// `ENTRIES` is sorted by `(module, item)` at build time (see build.rs), so
/// this is a plain binary search over a static table rather than a hash
/// lookup or a runtime parse -- there is nothing to build or cache, and the
/// module/item strings looked up are already exactly what appears in the
/// source (an import's module path, a prelude binding's item name).
pub(crate) fn lookup(module: &str, item: &str) -> &'static [DocEntry] {
    let key = (module, item);
    let start = ENTRIES.partition_point(|entry| (entry.module, entry.item) < key);
    let len = ENTRIES[start..].partition_point(|entry| (entry.module, entry.item) == key);
    &ENTRIES[start..start + len]
}

/// A `def name args` / `class Name` / `let name` style signature, noted with
/// its module, and written in its class when it's a member.
///
/// Reconstructed from the JSON's parameter names rather than quoted from
/// source, since the JSON has no exact source span for a whole signature --
/// this is an approximation, good enough for a hover title, not a promise
/// that it matches the declaration verbatim.
pub(crate) fn signature(entry: &DocEntry) -> String {
    let (class, short_name) = match entry.item.rsplit_once('.') {
        Some((class, name)) => (Some(class), name),
        None if entry.item.is_empty() => return format!("import {}", entry.module),
        None => (None, entry.item),
    };
    let named = |name: &str, binders: &[&str]| match binders {
        [] => name.to_owned(),
        _ => format!("{name}[{}]", binders.join(", ")),
    };
    let name = named(short_name, entry.binders);
    let label = match entry.kind {
        "class" => {
            let decorators: String = entry
                .decorators
                .iter()
                .map(|decorator| format!("#[{decorator}]\n"))
                .collect();
            format!("{decorators}class {name}")
        }
        "field" => signature::binding(&format!("field {short_name}"), entry.type_),
        "value" => signature::variable(&format!("let {short_name}"), None),
        "alias" => format!("@let {name} = {}", entry.type_.unwrap_or("...")),
        _ => {
            let params: Vec<_> = entry
                .params
                .iter()
                .map(|param| signature::Param {
                    head: param
                        .name
                        .strip_suffix('?')
                        .unwrap_or(param.name)
                        .to_owned(),
                    ty: param.type_.map(str::to_owned),
                    default: param.optional,
                })
                .collect();
            signature::def(&format!("def {name}"), &params, entry.type_, true)
        }
    };
    // A class's header, with the binders its own entry gives
    let class = class.map(|class| {
        let binders = lookup(entry.module, class)
            .first()
            .map_or(&[][..], |owner| owner.binders);
        format!("class {}", named(class, binders))
    });
    signature::member(Some(entry.module), class.as_deref(), &label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_sorted_for_binary_search() {
        assert!(
            ENTRIES
                .windows(2)
                .all(|pair| (pair[0].module, pair[0].item) <= (pair[1].module, pair[1].item)),
            "doc_index::ENTRIES must stay sorted by (module, item) for lookup's binary search"
        );
    }

    #[test]
    fn signatures_include_annotated_types() {
        let function = DocEntry {
            module: "m",
            item: "place",
            kind: "function",
            doc: "",
            binders: &["T", ":K", "...R"],
            decorators: &[],
            params: &[
                Param {
                    name: "point",
                    optional: false,
                    type_: Some("Point"),
                },
                Param {
                    name: ":limit",
                    optional: true,
                    type_: None,
                },
            ],
            type_: Some("(Int | nil)"),
        };
        let text = signature(&function);
        assert_eq!(
            text,
            "# module m\ndef place[T, :K, ...R] point@Point :limit = ... -> (Int | nil)\n  ..."
        );
        signature::assert_parses(&text);
        let field = DocEntry {
            module: "m",
            item: "Point.x",
            kind: "field",
            doc: "",
            binders: &[],
            decorators: &[],
            params: &[],
            type_: Some("Int"),
        };
        let text = signature(&field);
        assert_eq!(text, "# module m\nclass Point\n  field x @ Int");
        signature::assert_parses(&text);

        let class = DocEntry {
            module: "m",
            item: "Box",
            kind: "class",
            doc: "",
            binders: &["T"],
            decorators: &[],
            params: &[],
            type_: None,
        };
        assert_eq!(signature(&class), "# module m\nclass Box[T]");

        let decorated = DocEntry {
            item: "Sym",
            binders: &[],
            decorators: &["final", "mixable"],
            ..class
        };
        let text = signature(&decorated);
        assert_eq!(text, "# module m\n#[final]\n#[mixable]\nclass Sym");
        signature::assert_parses(&text);

        let module = DocEntry {
            item: "",
            kind: "module",
            ..class
        };
        assert_eq!(signature(&module), "import m");
        let value = DocEntry {
            item: "args",
            kind: "value",
            ..class
        };
        assert_eq!(signature(&value), "# module m\nlet args = ...");
    }
}
