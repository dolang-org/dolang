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
    params: Vec<ParamJson>,
    #[serde(default)]
    members: Vec<Entity>,
    #[serde(default)]
    module: String,
    #[serde(default)]
    item: String,
}

#[derive(Clone, serde::Deserialize)]
struct ParamJson {
    name: String,
    #[serde(default)]
    optional: bool,
}

/// One row of the generated table, before Rust-literal rendering.
struct Row {
    module: String,
    item: String,
    kind: &'static str,
    doc: String,
    params: Vec<ParamJson>,
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
        let module: ModuleJson = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
        modules.insert(module.module.clone(), module);
    }
    for module in modules.values() {
        add_module(&mut rows, &modules, module);
    }
    // Sorted by (module, item) so doc_index::lookup can binary search it.
    rows.sort_by(|a, b| (&a.module, &a.item).cmp(&(&b.module, &b.item)));
    rows
}

fn add_module(rows: &mut Vec<Row>, modules: &HashMap<String, ModuleJson>, module: &ModuleJson) {
    rows.push(Row {
        module: module.module.clone(),
        item: String::new(),
        kind: "module",
        doc: doc_markdown::remove_manual_anchors(module.doc.as_deref().unwrap_or_default()),
        params: Vec::new(),
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
        doc: doc_markdown::remove_manual_anchors(entity.doc.as_deref().unwrap_or_default()),
        params: entity.params.clone(),
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
            "    DocEntry {{ module: {:?}, item: {:?}, kind: {:?}, doc: {:?}, params: &[",
            row.module, row.item, row.kind, row.doc,
        )
        .expect("writing to a String cannot fail");
        for param in &row.params {
            write!(
                out,
                "Param {{ name: {:?}, optional: {} }}, ",
                param.name, param.optional,
            )
            .expect("writing to a String cannot fail");
        }
        out.push_str("] },\n");
    }
    out.push_str("];\n");
    out
}
