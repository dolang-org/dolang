//! Bakes the per-module doc JSON dump (see `extract_doc_json` in dodo.dol)
//! into a static table for `src/doc_index.rs`, keyed by `(module, item)` and
//! sorted for binary search.
//!
//! `DOLANG_LSP_DOC_JSON_DIR` names the directory to read; a plain `cargo
//! build`/`dodo build` doesn't set it (extraction requires a working
//! `dolang` binary and is comparatively slow), so that case just emits an
//! empty table rather than failing the build.

use std::{collections::HashMap, env, fmt::Write as _, fs, path::PathBuf};

mod doc_markdown;

// `dolang -m compile extract --doc` nests the cooked documentation
// projection this build script wants under "doc", alongside the raw
// nodes/tokens/diagnostics dump it doesn't.
#[derive(serde::Deserialize)]
struct ExtractJson {
    doc: ModuleJson,
}

#[derive(serde::Deserialize)]
struct ModuleJson {
    module: String,
    #[serde(default)]
    doc: Option<String>,
    #[serde(default)]
    entities: Vec<Entity>,
}

#[derive(Clone, serde::Deserialize)]
struct Entity {
    kind: String,
    name: String,
    #[serde(default, rename = "pub")]
    is_pub: bool,
    #[serde(default)]
    doc: Option<String>,
    #[serde(default)]
    binders: Vec<String>,
    #[serde(default)]
    params: Vec<ParamJson>,
    #[serde(default)]
    members: Vec<Entity>,
    #[serde(default)]
    module: String,
    #[serde(default)]
    item: String,
    /// A field's annotated type
    #[serde(default, rename = "type")]
    type_: Option<TypeJson>,
    /// A function or method's annotated return type
    #[serde(default)]
    returns: Option<TypeJson>,
}

#[derive(Clone, serde::Deserialize)]
struct ParamJson {
    name: String,
    #[serde(default)]
    optional: bool,
    #[serde(default, rename = "type")]
    type_: Option<TypeJson>,
}

/// An annotated type, as the tree the extractor gives
#[derive(Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum TypeJson {
    Name {
        name: String,
    },
    Const {
        text: String,
    },
    App {
        base: Box<TypeJson>,
        args: Vec<TypeArgJson>,
    },
    Schema {
        args: Vec<TypeArgJson>,
    },
    Union {
        members: Vec<TypeJson>,
    },
    Func {
        params: Vec<TypeArgJson>,
        ret: Box<TypeJson>,
    },
}

/// An item in the `[]`, `()` or `{}` of a type
#[derive(Clone, serde::Deserialize)]
struct TypeArgJson {
    kind: String,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    key: Option<String>,
    #[serde(rename = "type")]
    ty: Option<TypeJson>,
}

/// How tightly a type form binds, so that it is parenthesized where it would have
/// to be in source
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Binding {
    Func,
    Union,
    Compact,
}

impl TypeJson {
    /// The type as written in a position binding as tightly as `context`
    fn render(&self, context: Binding) -> String {
        let (text, binding) = match self {
            TypeJson::Name { name } => return name.clone(),
            TypeJson::Const { text } => return text.clone(),
            TypeJson::App { base, args } => (
                format!("{}[{}]", base.render(Binding::Compact), render_args(args)),
                Binding::Compact,
            ),
            TypeJson::Schema { args } => (format!("{{{}}}", render_args(args)), Binding::Compact),
            TypeJson::Union { members } => (
                members
                    .iter()
                    .map(|member| member.render(Binding::Compact))
                    .collect::<Vec<_>>()
                    .join(" | "),
                Binding::Union,
            ),
            TypeJson::Func { params, ret } => (
                format!("({}) -> {}", render_args(params), ret.render(Binding::Func)),
                Binding::Func,
            ),
        };
        if binding < context {
            format!("({text})")
        } else {
            text
        }
    }
}

fn render_args(args: &[TypeArgJson]) -> String {
    args.iter()
        .map(|arg| {
            let optional = if arg.optional { "?" } else { "" };
            let Some(ty) = arg.ty.as_ref() else {
                return format!("{optional}...");
            };
            let ty = ty.render(Binding::Func);
            match (arg.kind.as_str(), &arg.key) {
                ("rest", _) => format!("{optional}...{ty}"),
                ("key", Some(key)) => format!("{optional}{key}: {ty}"),
                _ => format!("{optional}{ty}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One row of the generated table, before Rust-literal rendering.
struct Row {
    module: String,
    item: String,
    kind: &'static str,
    doc: String,
    binders: Vec<String>,
    params: Vec<ParamJson>,
    type_: Option<String>,
}

fn main() {
    println!("cargo:rerun-if-env-changed=DOLANG_LSP_DOC_JSON_DIR");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));
    let out_path = out_dir.join("doc_index_data.rs");

    let rows = match env::var("DOLANG_LSP_DOC_JSON_DIR") {
        Ok(dir) => {
            println!("cargo:rerun-if-changed={dir}");
            collect_rows(&dir)
        }
        Err(_) => Vec::new(),
    };

    fs::write(&out_path, render(&rows)).expect("failed to write generated doc index");
}

fn collect_rows(dir: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut modules = HashMap::new();
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read DOLANG_LSP_DOC_JSON_DIR {dir}: {e}"));
    for entry in entries {
        let path = entry.expect("failed to read directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let module: ModuleJson = serde_json::from_str::<ExtractJson>(&text)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
            .doc;
        modules.insert(module.module.clone(), module);
    }
    for module in modules.values() {
        add_module(&mut rows, &modules, module);
    }
    // Sorted by (module, item) so doc_index::lookup can binary search it.
    rows.sort_by(|a, b| (&a.module, &a.item).cmp(&(&b.module, &b.item)));
    rows
}

/// Converts extracted doc Markdown, written for the mkdocs site, for hover.
fn hover_doc(doc: Option<&str>) -> String {
    let doc = doc_markdown::remove_manual_anchors(doc.unwrap_or_default());
    doc_markdown::remove_playground_markup(&doc)
}

fn add_module(rows: &mut Vec<Row>, modules: &HashMap<String, ModuleJson>, module: &ModuleJson) {
    rows.push(Row {
        module: module.module.clone(),
        item: String::new(),
        kind: "module",
        doc: hover_doc(module.doc.as_deref()),
        binders: Vec::new(),
        params: Vec::new(),
        type_: None,
    });
    for entity in &module.entities {
        if entity.is_pub {
            let entity = resolve_entity(modules, &module.module, entity, &mut Vec::new());
            add_entity(rows, &module.module, "", &entity);
        }
    }
}

fn resolve_entity(
    modules: &HashMap<String, ModuleJson>,
    module: &str,
    entity: &Entity,
    chain: &mut Vec<(String, String)>,
) -> Entity {
    if entity.kind == "import_module" {
        let mut result = entity.clone();
        result.kind = "value".to_owned();
        return result;
    }
    if entity.kind != "import_item" {
        return entity.clone();
    }
    let key = (module.to_owned(), entity.name.clone());
    if chain.contains(&key) {
        chain.push(key);
        let path = chain
            .iter()
            .map(|(module, item)| format!("{module}.{item}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        panic!("cyclic public doc re-export: {path}");
    }
    chain.push(key);
    let source = modules.get(&entity.module).unwrap_or_else(|| {
        let path = chain
            .iter()
            .map(|(module, item)| format!("{module}.{item}"))
            .chain(std::iter::once(entity.module.clone()))
            .collect::<Vec<_>>()
            .join(" -> ");
        panic!(
            "public doc re-export names missing module '{}': {path}",
            entity.module,
        )
    });
    let target = source
        .entities
        .iter()
        .find(|candidate| candidate.name == entity.item)
        .unwrap_or_else(|| {
            let path = chain
                .iter()
                .map(|(module, item)| format!("{module}.{item}"))
                .chain(std::iter::once(format!(
                    "{}.{}",
                    entity.module, entity.item
                )))
                .collect::<Vec<_>>()
                .join(" -> ");
            panic!(
                "public doc re-export names missing item '{}.{}': {path}",
                entity.module, entity.item
            )
        });
    let mut result = resolve_entity(modules, &entity.module, target, chain);
    chain.pop();
    result.name.clone_from(&entity.name);
    result.is_pub = entity.is_pub;
    result
}

/// A class's own entry uses its bare name as `item`; its members are dotted
/// onto that (`Class.method`, `Class.(init)`) -- the same qualified-name
/// convention the mkdocs-autorefs cross-references in docs/ use.
fn add_entity(rows: &mut Vec<Row>, module: &str, prefix: &str, entity: &Entity) {
    let item = if prefix.is_empty() {
        entity.name.clone()
    } else {
        format!("{prefix}.{}", entity.name)
    };
    rows.push(Row {
        module: module.to_owned(),
        item: item.clone(),
        kind: match entity.kind.as_str() {
            "class" => "class",
            "function" => "function",
            "method" => "method",
            "field" => "field",
            _ => "value",
        },
        doc: hover_doc(entity.doc.as_deref()),
        binders: entity.binders.clone(),
        params: entity.params.clone(),
        type_: entity
            .returns
            .as_ref()
            .or(entity.type_.as_ref())
            .map(|ty| ty.render(Binding::Compact)),
    });
    for member in &entity.members {
        if member.is_pub {
            add_entity(rows, module, &item, member);
        }
    }
}

fn render(rows: &[Row]) -> String {
    let mut out = String::from("pub(crate) static ENTRIES: &[DocEntry] = &[\n");
    for row in rows {
        write!(
            out,
            "    DocEntry {{ module: {:?}, item: {:?}, kind: {:?}, doc: {:?}, binders: &{:?}, type_: {:?}, params: &[",
            row.module, row.item, row.kind, row.doc, row.binders, row.type_,
        )
        .expect("writing to a String cannot fail");
        for param in &row.params {
            write!(
                out,
                "Param {{ name: {:?}, optional: {}, type_: {:?} }}, ",
                param.name,
                param.optional,
                param.type_.as_ref().map(|ty| ty.render(Binding::Compact)),
            )
            .expect("writing to a String cannot fail");
        }
        out.push_str("] },\n");
    }
    out.push_str("];\n");
    out
}
