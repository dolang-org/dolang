//! Static per-module documentation index, baked in at build time from the
//! same per-module JSON dump `dodo mkdocs` produces (see `extract_doc_json`
//! in dodo.dol and `build.rs`). Used by hover to describe an identifier that
//! names something outside the file being edited -- an imported or prelude
//! item -- which has no declaration in this document to read a doc comment
//! from.

#[cfg(test)]
#[path = "../doc_markdown.rs"]
mod doc_markdown;

/// One parameter of a documented function or method.
pub(crate) struct Param {
    pub(crate) name: &'static str,
    pub(crate) optional: bool,
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
    pub(crate) params: &'static [Param],
}

// The real table depends on DOLANG_LSP_DOC_JSON_DIR having been set at build
// time (see build.rs), which is only true for `dodo install` -- an ordinary
// `cargo test` run gets an empty table and could never exercise a lookup.
// Tests get a small fixed fixture instead, independent of that environment.
#[cfg(not(test))]
include!(concat!(env!("OUT_DIR"), "/doc_index_data.rs"));

#[cfg(test)]
pub(crate) static ENTRIES: &[DocEntry] = &[DocEntry {
    module: "term",
    item: "echo",
    kind: "function",
    doc: "(nil) Writes to standard output.",
    params: &[Param {
        name: "...args",
        optional: false,
    }],
}];

/// Looks up a documented module/item pair.
///
/// `ENTRIES` is sorted by `(module, item)` at build time (see build.rs), so
/// this is a plain binary search over a static table rather than a hash
/// lookup or a runtime parse -- there is nothing to build or cache, and the
/// module/item strings looked up are already exactly what appears in the
/// source (an import's module path, a prelude binding's item name).
pub(crate) fn lookup(module: &str, item: &str) -> Option<&'static DocEntry> {
    ENTRIES
        .binary_search_by(|entry| (entry.module, entry.item).cmp(&(module, item)))
        .ok()
        .map(|index| &ENTRIES[index])
}

/// A one-line `def name args` / `class Name` / `let name` style signature.
///
/// Reconstructed from the JSON's parameter names rather than quoted from
/// source, since the JSON has no exact source span for a whole signature --
/// this is an approximation, good enough for a hover title, not a promise
/// that it matches the declaration verbatim.
pub(crate) fn signature(entry: &DocEntry) -> String {
    let short_name = entry
        .item
        .rsplit('.')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(entry.module);
    match entry.kind {
        "module" => format!("module {}", entry.module),
        "class" => format!("class {short_name}"),
        "field" => format!("field {short_name}"),
        "value" => format!("let {short_name}"),
        _ => {
            let params: Vec<String> = entry
                .params
                .iter()
                .map(|param| {
                    if param.optional
                        && !param.name.ends_with('?')
                        && !param.name.starts_with("...")
                    {
                        format!("{}?", param.name)
                    } else {
                        param.name.to_owned()
                    }
                })
                .collect();
            if params.is_empty() {
                format!("def {short_name}")
            } else {
                format!("def {short_name} {}", params.join(" "))
            }
        }
    }
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
}
