//! Declarations found by module and name.
//!
//! A caller resolves a module's name to its unit, with what it keeps of the
//! module as `M`, or to nothing when it has no source, which leaves the
//! module's items as bundled ones that it documents some other way.
use dolang_compile::{Kind, NodeId, Unit};

use crate::unit::is_overload;

/// The most re-exports a lookup follows, which ends a cycle of them
const MAX_HOPS: usize = 16;

/// Where a lookup ended
pub enum Target<'u, M> {
    /// A declaration in a module with a unit
    Source {
        unit: &'u Unit<'u>,
        module: M,
        /// The module's name
        name: String,
        id: NodeId,
    },
    /// An item of a module with no unit, by module and name: `item` empty
    /// for the module itself, and dotted as `Class.member` for a class member
    Bundled { module: String, item: String },
}

/// The item `module` exports as `name`, following re-exports, among the
/// modules `resolve` finds. An overloaded function is found as its
/// implementation.
pub fn item<'u, M>(
    resolve: impl Fn(&str) -> Option<(&'u Unit<'u>, M)>,
    module: &str,
    name: &str,
) -> Option<Target<'u, M>> {
    let (mut module, mut name) = (module.to_owned(), name.to_owned());
    for _ in 0..MAX_HOPS {
        let Some((unit, found)) = resolve(&module) else {
            return Some(Target::Bundled { module, item: name });
        };
        let named = |span: &_| unit.text(*span) == name;
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
                } if named(&span) => {
                    return Some(Target::Source {
                        unit,
                        module: found,
                        name: module,
                        id,
                    });
                }
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
            return Some(Target::Source {
                unit,
                module: found,
                name: module,
                id,
            });
        }
        let (next, item) = reexport?;
        (module, name) = (unit.text(next).to_owned(), unit.text(item).to_owned());
    }
    None
}

/// The member `name` of the class `module` declares as `class`, among the
/// modules `resolve` finds. A special method's name keeps its parentheses,
/// and an overloaded method is found as its implementation.
pub fn member<'u, M>(
    resolve: impl Fn(&str) -> Option<(&'u Unit<'u>, M)>,
    module: &str,
    class: &str,
    name: &str,
) -> Option<Target<'u, M>> {
    let Some((unit, found)) = resolve(module) else {
        return Some(Target::Bundled {
            module: module.to_owned(),
            item: format!("{class}.{name}"),
        });
    };
    let named = |id: NodeId, name: &str| {
        let node = unit.node(id);
        let span = node.and_then(|node| node.definition());
        span.is_some_and(|span| unit.text(span) == name)
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
    Some(Target::Source {
        unit,
        module: found,
        name: module.to_owned(),
        id,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use dolang_compile::{Config, Mode};

    use super::*;
    use crate::unit::{overload, overloads};

    fn modules(sources: &[(&str, &str)]) -> HashMap<String, Unit<'static>> {
        sources
            .iter()
            .map(|&(name, text)| {
                let mut config = Config::new();
                config.document(true).mode(Mode::Module {
                    name: name.to_owned().into(),
                });
                let path = format!("/{name}.dol");
                let unit = config.unit(Path::new(&path), text.to_owned());
                (name.to_owned(), unit)
            })
            .collect()
    }

    /// Finds a module's unit among `modules`, by name
    fn resolver<'m>(
        modules: &'m HashMap<String, Unit<'static>>,
    ) -> impl Fn(&str) -> Option<(&'m Unit<'m>, ())> + 'm {
        move |name| modules.get(name).map(|unit| (unit, ()))
    }

    /// The module and the name a lookup found
    fn found(
        modules: &HashMap<String, Unit<'static>>,
        module: &str,
        name: &str,
    ) -> Option<(String, String)> {
        match item(resolver(modules), module, name)? {
            Target::Source { unit, name, id, .. } => {
                let span = unit.node(id)?.definition()?;
                Some((name, unit.text(span).to_owned()))
            }
            Target::Bundled { module, item } => Some((module, item)),
        }
    }

    #[test]
    fn items_and_reexports() {
        let modules = modules(&[
            ("a", "pub def f x\n  x\ndef g x\n  x\n"),
            ("re", "pub import a:\n  - f\npub import std:\n  - Int\n"),
        ]);
        let pair = |module: &str, name: &str| Some((module.to_owned(), name.to_owned()));
        assert_eq!(found(&modules, "a", "f"), pair("a", "f"));
        assert_eq!(found(&modules, "re", "f"), pair("a", "f"));
        // A private item isn't exported
        assert_eq!(found(&modules, "a", "g"), None);
        // A module with no source is bundled
        assert_eq!(found(&modules, "re", "Int"), pair("std", "Int"));
    }

    #[test]
    fn overloads_find_the_implementation() {
        let modules = modules(&[(
            "a",
            "@def f x @ Int -> Int\n@def f x @ Str -> Str\npub def f x\n  x\n",
        )]);
        let Some(Target::Source { unit, id, .. }) = item(resolver(&modules), "a", "f") else {
            panic!("expected a source declaration");
        };
        let node = unit.node(id).unwrap();
        assert!(matches!(
            node.kind(),
            Kind::Function {
                type_only: false,
                ..
            }
        ));
        // Its overloads in source order, and an overload has none of its own
        let found = overloads(unit, id);
        let lines: Vec<_> = (found.iter())
            .map(|&id| {
                let span = unit.node(id).unwrap().definition().unwrap();
                span.start().line_number()
            })
            .collect();
        assert_eq!(lines, [1, 2]);
        assert_eq!(overloads(unit, found[0]), []);
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
    fn line(target: Option<Target<'_, ()>>, chosen: Option<u32>) -> Option<(String, u32)> {
        let Target::Source { unit, name, id, .. } = target? else {
            return None;
        };
        let id = chosen.map_or(id, |index| overload(unit, id, index));
        let span = unit.node(id)?.definition()?;
        Some((name, span.start().line_number()))
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
        let at = |name, chosen| line(member(resolver(&modules), "a", "A", name), chosen);
        let pair = |line| Some(("a".to_owned(), line));
        assert_eq!(at("x", None), pair(2));
        // A special method keeps its parentheses
        assert_eq!(at("(init)", None), pair(3));
        assert_eq!(at("init", None), None);
        // An overloaded method is its implementation, until a call chooses
        assert_eq!(at("get", None), pair(7));
        assert_eq!(at("get", Some(1)), pair(6));
        assert_eq!(at("get", Some(2)), pair(7));
        let missing = member(resolver(&modules), "a", "B", "x");
        assert_eq!(line(missing, None), None);
        // A class with no source is bundled
        let Some(Target::Bundled { module, item }) =
            member(resolver(&modules), "std", "Str", "len")
        else {
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
        let at = |chosen| line(member(resolver(&modules), "a", "P", "area"), chosen);
        // A protocol's own member is written without `@`
        assert_eq!(at(None), Some(("a".to_owned(), 3)));
        assert_eq!(at(Some(0)), Some(("a".to_owned(), 2)));
    }
}
