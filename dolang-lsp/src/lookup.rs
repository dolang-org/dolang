//! Declarations in the modules a check saw, found by module and name.
//!
//! A check keeps the units of the modules it read from source, so what's found
//! is what the check's analysis was computed against.  A module it read from a
//! bundled typelib has no source here, and is left to the static doc index.

use std::{collections::HashMap, sync::Arc};

use dolang_compile::{Kind, NodeId, Unit};
use tower_lsp_server::ls_types::Uri;

use crate::backend::span_text;

/// The most re-exports a lookup follows, which ends a cycle of them
const MAX_HOPS: usize = 16;

/// A module a check read from source
pub(crate) struct Module {
    pub(crate) uri: Uri,
    pub(crate) unit: Arc<Unit<'static>>,
}

/// The modules a check read from source, by name
pub(crate) type Modules = HashMap<String, Module>;

/// Where a lookup ended
pub(crate) enum Target<'m> {
    /// A declaration in a module read from source
    Source(&'m Module, NodeId),
    /// An item of a module with no source, by module and name
    Bundled(String, String),
}

/// The item a module exports as `name`, following re-exports. An overloaded
/// function is found as its implementation.
pub(crate) fn item<'m>(modules: &'m Modules, module: &str, name: &str) -> Option<Target<'m>> {
    let (mut module, mut name) = (module.to_owned(), name.to_owned());
    for _ in 0..MAX_HOPS {
        let Some(found) = modules.get(&module) else {
            return Some(Target::Bundled(module, name));
        };
        let unit = &found.unit;
        let source = unit.source();
        let named = |span: &_| span_text(source, span) == name;
        let mut signature = None;
        let mut reexport = None;
        for id in unit.node(unit.root()?)?.children() {
            match unit.node(id)?.kind() {
                Kind::Class {
                    name: span,
                    is_pub: true,
                    ..
                }
                | Kind::Bind {
                    name: span,
                    is_pub: true,
                }
                | Kind::Alias {
                    name: span,
                    is_pub: true,
                    ..
                }
                | Kind::Function {
                    name: span,
                    is_pub: true,
                    type_only: false,
                } if named(&span) => return Some(Target::Source(found, id)),
                Kind::Function {
                    name: span,
                    is_pub: true,
                    type_only: true,
                } if named(&span) => signature = Some(id),
                Kind::ImportItem {
                    module,
                    item,
                    name: span,
                    is_pub: true,
                    ..
                } if named(&span) => reexport = Some((module, item)),
                _ => {}
            }
        }
        if let Some(id) = signature {
            return Some(Target::Source(found, id));
        }
        let (next, item) = reexport?;
        (module, name) = (
            span_text(source, &next).to_owned(),
            span_text(source, &item).to_owned(),
        );
    }
    None
}

/// The member `name` of the class a module declares as `class`. A special
/// method's name keeps its parentheses, and an overloaded method is found as
/// its implementation.
pub(crate) fn member<'m>(
    modules: &'m Modules,
    module: &str,
    class: &str,
    name: &str,
) -> Option<Target<'m>> {
    let Some(found) = modules.get(module) else {
        return Some(Target::Bundled(
            module.to_owned(),
            format!("{class}.{name}"),
        ));
    };
    let unit = &found.unit;
    let named = |id: NodeId, name: &str| {
        let node = unit.node(id);
        let span = node.and_then(|node| node.definition());
        span.is_some_and(|span| span_text(unit.source(), &span) == name)
    };
    let class = unit.node(unit.root()?)?.children().find(|&id| {
        let is_class = matches!(
            unit.node(id).map(|node| node.kind()),
            Some(Kind::Class { .. })
        );
        is_class && named(id, class)
    })?;
    let id = unit.node(class)?.children().find(|&id| {
        let member = matches!(
            unit.node(id).map(|node| node.kind()),
            Some(Kind::Method { .. } | Kind::SpecialMethod { .. } | Kind::Field { .. })
        );
        member && named(id, name) && !is_overload(unit, id)
    })?;
    Some(Target::Source(found, id))
}

/// The `index`th `@def` overload of the function or method `id`, in source
/// order, or `id` itself when it has no such overload
pub(crate) fn overload(unit: &Unit<'_>, id: NodeId, index: u32) -> NodeId {
    let chosen = || {
        let node = unit.node(id)?;
        let name = span_text(unit.source(), &node.definition()?);
        let mut overloads: Vec<_> = unit
            .node(node.parent()?)?
            .children()
            .filter(|&other| is_overload(unit, other))
            .filter_map(|other| {
                let node = unit.node(other)?;
                let named = span_text(unit.source(), &node.definition()?) == name;
                named.then_some((node.span()?.start().byte_offset(), other))
            })
            .collect();
        overloads.sort_unstable_by_key(|&(start, _)| start);
        Some(overloads.get(usize::try_from(index).ok()?)?.1)
    };
    chosen().unwrap_or(id)
}

/// Whether a declaration is an `@def` overload signature
fn is_overload(unit: &Unit<'_>, id: NodeId) -> bool {
    let Some(node) = unit.node(id) else {
        return false;
    };
    match node.kind() {
        Kind::Function { type_only, .. } => type_only,
        // A protocol's own member is type-only too, but is written without `@`
        Kind::Method {
            type_only: true, ..
        }
        | Kind::SpecialMethod {
            type_only: true, ..
        } => {
            let (Some(span), Some(name)) = (node.span(), node.definition()) else {
                return false;
            };
            let before = unit
                .source()
                .get(span.start().byte_offset()..name.start().byte_offset());
            let def = before.and_then(|before| before.trim_end().strip_suffix("def"));
            def.is_some_and(|def| def.trim_end().ends_with('@'))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::{Config, Mode};

    use super::*;

    fn modules(sources: &[(&str, &str)]) -> Modules {
        sources
            .iter()
            .map(|&(name, text)| {
                let mut config = Config::new();
                config.document(true).mode(Mode::Module {
                    name: name.to_owned().into(),
                });
                let path = format!("/{name}.dol");
                let unit = config.unit(Path::new(&path), text.to_owned());
                let uri = format!("file://{path}").parse().unwrap();
                let module = Module {
                    uri,
                    unit: Arc::new(unit),
                };
                (name.to_owned(), module)
            })
            .collect()
    }

    /// The module and the name a lookup found
    fn found(modules: &Modules, module: &str, name: &str) -> Option<(String, String)> {
        match item(modules, module, name)? {
            Target::Source(module, id) => {
                let unit = &module.unit;
                let span = unit.node(id)?.definition()?;
                let name = span_text(unit.source(), &span).to_owned();
                Some((module.uri.path().to_string(), name))
            }
            Target::Bundled(module, name) => Some((module, name)),
        }
    }

    #[test]
    fn items_and_reexports() {
        let modules = modules(&[
            ("a", "pub def f x\n  x\ndef g x\n  x\n"),
            ("re", "pub import a:\n  - f\npub import std:\n  - Int\n"),
        ]);
        let pair = |module: &str, name: &str| Some((module.to_owned(), name.to_owned()));
        assert_eq!(found(&modules, "a", "f"), pair("/a.dol", "f"));
        assert_eq!(found(&modules, "re", "f"), pair("/a.dol", "f"));
        // A private item isn't exported
        assert_eq!(found(&modules, "a", "g"), None);
        // A module with no source is left to the doc index
        assert_eq!(found(&modules, "re", "Int"), pair("std", "Int"));
    }

    #[test]
    fn overloads_find_the_implementation() {
        let modules = modules(&[(
            "a",
            "@def f x @ Int -> Int\n@def f x @ Str -> Str\npub def f x\n  x\n",
        )]);
        let Some(Target::Source(module, id)) = item(&modules, "a", "f") else {
            panic!("expected a source declaration");
        };
        let node = module.unit.node(id).unwrap();
        assert!(matches!(
            node.kind(),
            Kind::Function {
                type_only: false,
                ..
            }
        ));
    }

    #[test]
    fn reexport_cycles_end() {
        let modules = modules(&[
            ("a", "pub import b:\n  - f\n"),
            ("b", "pub import a:\n  - f\n"),
        ]);
        assert_eq!(found(&modules, "a", "f"), None);
    }

    /// The source line a lookup found's declaration is on, by its module
    fn line(target: Option<Target<'_>>, chosen: Option<u32>) -> Option<(String, usize)> {
        let Target::Source(module, id) = target? else {
            return None;
        };
        let id = chosen.map_or(id, |index| overload(&module.unit, id, index));
        let unit = &module.unit;
        let span = unit.node(id)?.definition()?;
        let line = unit.source()[..span.start().byte_offset()].lines().count();
        Some((module.uri.path().to_string(), line))
    }

    #[test]
    fn members() {
        let modules = modules(&[(
            "a",
            "\
class A
  pub field x = 0
  def (init) self
    nil
  @def get self x @ Int -> Int
  @def get self x @ Str -> Str
  pub def get self x
    x
",
        )]);
        let at = |name, chosen| line(member(&modules, "a", "A", name), chosen);
        let pair = |line| Some(("/a.dol".to_owned(), line));
        assert_eq!(at("x", None), pair(2));
        // A special method keeps its parentheses
        assert_eq!(at("(init)", None), pair(3));
        assert_eq!(at("init", None), None);
        // An overloaded method is its implementation, until a call chooses
        assert_eq!(at("get", None), pair(7));
        assert_eq!(at("get", Some(1)), pair(6));
        assert_eq!(at("get", Some(2)), pair(7));
        assert_eq!(line(member(&modules, "a", "B", "x"), None), None);
        // A class with no source is left to the doc index
        let Some(Target::Bundled(module, item)) = member(&modules, "std", "Str", "len") else {
            panic!("expected a bundled member");
        };
        assert_eq!((&module[..], &item[..]), ("std", "Str.len"));
    }

    #[test]
    fn protocol_overloads() {
        let modules = modules(&[(
            "a",
            "\
@class P
  @def area self x @ Int -> Int
  pub def area self x -> Int
",
        )]);
        let at = |chosen| line(member(&modules, "a", "P", "area"), chosen);
        // A protocol's own member is written without `@`
        assert_eq!(at(None), Some(("/a.dol".to_owned(), 3)));
        assert_eq!(at(Some(0)), Some(("/a.dol".to_owned(), 2)));
    }
}
