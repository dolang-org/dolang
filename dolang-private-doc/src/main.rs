#![deny(warnings)]

use std::{
    collections::HashMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use clap::Parser;
use dolang::compile::{Config, Kind, Node, NodeId, Span, Unit};
use serde_json::{Value, json};

#[derive(Parser)]
struct Cli {
    /// Module name for the output (used as identifier prefix)
    #[arg(long)]
    module: Option<String>,
    /// Include non-pub entities (default: pub only)
    #[arg(long)]
    all: bool,
    /// Source path (reads stdin if omitted)
    path: Option<PathBuf>,
}

/// Extract the text slice from source bytes for a given byte range
fn src_text(content: &[u8], start: usize, end: usize) -> &str {
    std::str::from_utf8(&content[start..end]).unwrap_or("")
}

fn span_text(content: &[u8], span: &Span) -> String {
    src_text(
        content,
        span.start().byte_offset(),
        span.end().byte_offset(),
    )
    .to_owned()
}

/// Scans backward for the preceding block of doc comments, skipping decorators.
///
/// Doc comments are the one thing the compiler does not model: they are just
/// comments, attached by adjacency rather than by syntax, so they are recovered
/// from the source text.
fn extract_doc(content: &[u8], offset: usize) -> String {
    let prefix = std::str::from_utf8(&content[..offset]).unwrap_or("");
    let prefix = prefix.rsplit_once('\n').map_or("", |(lines, _)| lines);
    let mut comment_lines: Vec<String> = Vec::new();
    let mut lines = prefix.lines().rev().peekable();

    while lines
        .peek()
        .is_some_and(|line| line.trim_start().starts_with("#["))
    {
        lines.next();
    }
    for line in lines {
        let line = line.trim_start();
        if !line.starts_with('#') || line.starts_with("#[") {
            break;
        }
        let stripped = line
            .strip_prefix("# ")
            .or_else(|| line.strip_prefix('#'))
            .unwrap_or(line);
        comment_lines.push(stripped.to_owned());
    }
    comment_lines.reverse();
    comment_lines.join("\n")
}

/// The prelude bindings whose presence as a decorator means something here.
///
/// Field scope and `#[getter]` are both decided by which prelude item a
/// decorator resolves to — the same test the elaborator applies — so resolving
/// these once lets every later check be an identity comparison rather than a
/// match on source text.
#[derive(Default)]
struct Decorators {
    getter: Option<NodeId>,
    class: Option<NodeId>,
    r#static: Option<NodeId>,
}

impl Decorators {
    fn collect(unit: &Unit<'_>) -> Self {
        let mut found = Self::default();
        for (id, node) in unit.nodes() {
            if let Kind::PreludeItem { module, item, .. } = node.kind()
                && module == "std"
            {
                match item {
                    "getter" => found.getter = Some(id),
                    "class" => found.class = Some(id),
                    "static" => found.r#static = Some(id),
                    _ => {}
                }
            }
        }
        found
    }
}

/// A declaration, with the child nodes that describe it.
struct Entity<'a> {
    node: Node<'a>,
    /// Nodes whose parent is this one, in source order
    children: Vec<(NodeId, Node<'a>)>,
}

impl Entity<'_> {
    fn decorates(&self, target: Option<NodeId>) -> bool {
        target.is_some()
            && self.children.iter().any(
                |(_, child)| matches!(child.kind(), Kind::Decorator { target: t } if t == target),
            )
    }

    /// The parameters of a function or method, in the order declared.
    ///
    /// `self` is excluded: it is an artifact of how methods are called, not part
    /// of the documented signature.
    fn params(&self, content: &[u8]) -> Value {
        Value::Array(
            self.children
                .iter()
                .filter_map(|(_, child)| match child.kind() {
                    Kind::PositionalParam { name, default } => {
                        let name = span_text(content, &name);
                        Some(json!({"name": name, "optional": default.is_some()}))
                    }
                    Kind::KeyParam { key, default, .. } => {
                        let name = format!(":{}", span_text(content, &key));
                        Some(json!({"name": name, "optional": default.is_some()}))
                    }
                    Kind::RestParam { name } => {
                        let bound = name.map_or(String::new(), |name| span_text(content, &name));
                        Some(json!({"name": format!("...{bound}"), "optional": false}))
                    }
                    _ => None,
                })
                .collect(),
        )
    }

    fn supers(&self, content: &[u8]) -> Value {
        let Kind::Class { supers, .. } = self.node.kind() else {
            return Value::Array(vec![]);
        };
        Value::Array(
            supers
                .map(|super_ref| json!(span_text(content, &super_ref.span)))
                .collect(),
        )
    }

    fn span_json(&self, name: &Span) -> Value {
        json!({
            "line": name.start().line_offset(),
            "col": name.start().column_offset(),
            "offset": name.start().byte_offset(),
        })
    }
}

/// Group nodes by parent so each declaration can be described by its children.
fn index_children<'a>(unit: &'a Unit<'a>) -> HashMap<NodeId, Vec<(NodeId, Node<'a>)>> {
    let mut children: HashMap<NodeId, Vec<(NodeId, Node<'a>)>> = HashMap::new();
    for (id, node) in unit.nodes() {
        if let Some(parent) = node.parent() {
            children.entry(parent).or_default().push((id, node));
        }
    }
    // Nothing depends on the order nodes are yielded, so impose source order.
    for group in children.values_mut() {
        group.sort_by_key(|(_, node)| node.span().start().byte_offset());
    }
    children
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let (path, content) = if let Some(path) = &cli.path {
        (path.as_ref(), fs::read(path)?)
    } else {
        let mut content = vec![];
        io::stdin().read_to_end(&mut content)?;
        (Path::new("<stdin>"), content)
    };

    println!("{}", document(path, &content, cli.module, cli.all));
    Ok(())
}

/// Describe every documented declaration in one source file.
fn document(path: &Path, content: &[u8], module: Option<String>, all: bool) -> Value {
    let unit = Config::new().recover(true).unit(path, content);
    let decorators = Decorators::collect(&unit);
    let children = index_children(&unit);

    // Members are grouped under the class that is their parent.
    let mut class_members: HashMap<NodeId, Vec<Value>> = HashMap::new();
    // Top level entities, paired with their offset so they can be ordered by it.
    // A class is held back until its members have been collected.
    let mut top_level: Vec<(usize, Option<Value>, NodeId)> = Vec::new();

    let mut nodes: Vec<_> = unit.nodes().collect();
    nodes.sort_by_key(|(_, node)| node.span().start().byte_offset());

    for (id, node) in nodes {
        let entity = Entity {
            node,
            children: children.get(&id).cloned().unwrap_or_default(),
        };

        // A special method names itself after the protocol it implements
        // rather than after any source text, and it belongs to the type's
        // interface however it was declared — so it anchors at the whole
        // declaration and counts as public.
        let (kind, name, name_span, is_pub) = match node.kind() {
            Kind::Class { name, is_pub, .. } => ("class", span_text(content, &name), name, is_pub),
            Kind::Function { name, is_pub } => {
                ("function", span_text(content, &name), name, is_pub)
            }
            Kind::Method { name, is_pub } => ("method", span_text(content, &name), name, is_pub),
            // A special method's name is written in parentheses; the span
            // covers the protocol name alone, so restore them for display.
            Kind::SpecialMethod { name } => (
                "method",
                format!("({})", span_text(content, &name)),
                name,
                true,
            ),
            Kind::Field { name, is_pub } => ("field", span_text(content, &name), name, is_pub),
            Kind::Bind { name, is_pub } => ("value", span_text(content, &name), name, is_pub),
            _ => continue,
        };
        let special = matches!(node.kind(), Kind::SpecialMethod { .. });
        if !is_pub && !all {
            continue;
        }

        let offset = name_span.start().byte_offset();
        let doc = extract_doc(content, node.span().start().byte_offset());
        let span = entity.span_json(&name_span);

        match kind {
            "class" => {
                let obj = json!({
                    "kind": "class",
                    "name": name,
                    "pub": is_pub,
                    "span": span,
                    "doc": doc,
                    "supers": entity.supers(content),
                    "members": [],
                });
                top_level.push((offset, Some(obj), id));
            }
            "method" => {
                // A `#[getter]` method presents as a field: it is read like one
                // and documenting it as a method would be a lie about its use.
                let member = if entity.decorates(decorators.getter) {
                    json!({
                        "kind": "field",
                        "name": name,
                        "pub": is_pub,
                        "span": span,
                        "doc": doc,
                    })
                } else {
                    json!({
                        "kind": "method",
                        "name": name,
                        "pub": is_pub,
                        "special": special,
                        "span": span,
                        "doc": doc,
                        "params": entity.params(content),
                    })
                };
                if let Some(parent) = node.parent() {
                    class_members.entry(parent).or_default().push(member);
                }
            }
            "field" => {
                let member = json!({
                    "kind": "field",
                    "name": name,
                    "pub": is_pub,
                    "span": span,
                    "doc": doc,
                    "scope": field_scope(&entity, &decorators),
                });
                if let Some(parent) = node.parent() {
                    class_members.entry(parent).or_default().push(member);
                }
            }
            "function" => {
                let obj = json!({
                    "kind": "function",
                    "name": name,
                    "pub": is_pub,
                    "span": span,
                    "doc": doc,
                    "params": entity.params(content),
                });
                top_level.push((offset, Some(obj), id));
            }
            // Only a top-level binding is a module value; one nested inside a
            // function is a local, and the parent link is what tells them apart.
            "value" if node.parent().is_none() => {
                let obj = json!({
                    "kind": "value",
                    "name": name,
                    "pub": is_pub,
                    "span": span,
                    "doc": doc,
                });
                top_level.push((offset, Some(obj), id));
            }
            _ => {}
        }
    }

    let entities: Vec<Value> = top_level
        .into_iter()
        .filter_map(|(_, obj, id)| {
            let mut obj = obj?;
            if obj["kind"] == "class" {
                let members = class_members.remove(&id).unwrap_or_default();
                *obj.get_mut("members").unwrap() = Value::Array(members);
            }
            Some(obj)
        })
        .collect();

    json!({
        "source": path.to_string_lossy(),
        "module": module,
        "entities": entities,
    })
}

/// Which namespace a field belongs to.
///
/// The elaborator derives this from `#[class]` and `#[static]` decorators that
/// have no runtime meaning; the decorators resolve to prelude items, so the same
/// determination here is an identity comparison rather than a text match.
fn field_scope(entity: &Entity<'_>, decorators: &Decorators) -> &'static str {
    if entity.decorates(decorators.class) {
        "class"
    } else if entity.decorates(decorators.r#static) {
        "static"
    } else {
        "instance"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixture.dol";
    const GOLDEN: &str = "tests/fixture.json";

    fn describe(all: bool) -> Value {
        let content = fs::read(FIXTURE).expect("fixture should be readable");
        // Name the source by its bare file name so the golden does not record
        // where the checkout happens to live.
        document(
            Path::new("fixture.dol"),
            &content,
            Some("fixture".to_owned()),
            all,
        )
    }

    /// The whole documented shape of a source file, held against a golden.
    ///
    /// The JSON is consumed by the mkdocstrings handler and its templates, so a
    /// change here is a change to the rendered site; set `DOLANG_TEST_UPDATE=1`
    /// to rewrite the golden once the new shape is what was intended.
    #[test]
    fn documents_a_source_file() {
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&describe(false)).unwrap()
        );
        if std::env::var_os("DOLANG_TEST_UPDATE").is_some() {
            fs::write(GOLDEN, &actual).expect("golden should be writable");
            return;
        }
        let expected = fs::read_to_string(GOLDEN).expect("golden should be readable");
        assert_eq!(actual, expected);
    }

    /// Private declarations are documented only when asked for.
    #[test]
    fn private_declarations_appear_only_with_all() {
        let names = |all| -> Vec<String> {
            describe(all)["entities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entity| entity["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert!(!names(false).contains(&"internal".to_owned()));
        assert!(names(true).contains(&"internal".to_owned()));
        assert!(names(true).contains(&"secret".to_owned()));
    }
}
