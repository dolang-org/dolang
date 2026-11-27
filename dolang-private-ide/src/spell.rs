//! How a unit's source names the declarations a checked type refers to, so
//! the type can be written into it as an annotation.
use std::collections::HashMap;

use dolang_compile::{Kind, Unit, typeck};

/// The names a unit's source reaches declarations by, at its top level
pub struct Spelling<'u> {
    unit: &'u Unit<'u>,
    /// The unit's own module name, or for a script, its file stem
    own: String,
    /// The local name of each item the unit imports, by module and item
    items: HashMap<(String, String), String>,
    /// The path each module the unit imports whole is written by
    modules: HashMap<String, String>,
}

impl<'u> Spelling<'u> {
    /// The names `unit`'s top level reaches declarations by, `own` being the
    /// name a check gives its module
    pub fn new(unit: &'u Unit<'u>, own: &str) -> Self {
        let mut items = HashMap::new();
        let mut modules = HashMap::new();
        let root = unit.root().and_then(|root| unit.node(root));
        let children: Vec<_> = root
            .map(|root| root.children().collect())
            .unwrap_or_default();
        for id in children {
            match unit.node(id).map(|node| node.kind()) {
                Some(Kind::ImportItem {
                    module, item, name, ..
                }) => {
                    let key = (unit.text(module).to_owned(), unit.text(item).to_owned());
                    items
                        .entry(key)
                        .or_insert_with(|| unit.text(name).to_owned());
                }
                // A module imported as itself binds the head of its path,
                // which its whole path is written through
                Some(Kind::ImportModule { module, name, .. }) => {
                    let (module, name) = (unit.text(module), unit.text(name));
                    let bare = module.split('.').next() == Some(name);
                    let path = if bare { module } else { name };
                    (modules.entry(module.to_owned())).or_insert_with(|| path.to_owned());
                }
                _ => {}
            }
        }
        Self {
            unit,
            own: own.to_owned(),
            items,
            modules,
        }
    }

    /// How the unit's source names the item `name` of `module`, if it can:
    /// - its own top-level declaration by its name
    /// - an item it imports by the name it binds
    /// - an item of a module it imports whole through the module
    /// - a prelude item by its bare name, and another `std` item through the
    ///   prelude's `std`
    ///
    /// A declaration nested in another, or an item of a module the unit
    /// doesn't import, has none. A local that shadows a name isn't accounted
    /// for.
    pub fn spell(&self, module: &str, name: &str) -> Option<String> {
        if name.contains('.') {
            return None;
        }
        if module == self.own {
            return Some(name.to_owned());
        }
        let key = (module.to_owned(), name.to_owned());
        if let Some(local) = self.items.get(&key) {
            return Some(local.clone());
        }
        if let Some(path) = self.modules.get(module) {
            return Some(format!("{path}.{name}"));
        }
        if let Some(bare) = self.unit.prelude_name(module, name) {
            return Some(bare.to_owned());
        }
        (module == "std").then(|| format!("std.{name}"))
    }

    /// A judgment's type as an annotation at its span writes it, if one can
    /// (see [`Judged::written`](typeck::Judged::written))
    pub fn written(&self, judged: typeck::Judged<'_>) -> Option<String> {
        judged.written(&|declaration| self.spell(declaration.module(), declaration.name()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::{Config, Mode};

    use super::*;

    #[test]
    fn names_through_imports_and_the_prelude() {
        let mut config = Config::new();
        config
            .document(true)
            .mode(Mode::Module { name: "m".into() });
        config
            .prelude()
            .clear()
            .import_items("std")
            .item("Int")
            .commit();
        let source = "\
import json: j
import net.http
import toml:
  Value: Toml
pub class A
";
        let unit = config.unit(Path::new("m.dol"), source);
        let spelling = Spelling::new(&unit, "m");
        let spell = |module, name| spelling.spell(module, name);
        assert_eq!(spell("m", "A").as_deref(), Some("A"));
        assert_eq!(spell("m", "f.Inner"), None);
        assert_eq!(
            spell("json", "DecodeValue").as_deref(),
            Some("j.DecodeValue")
        );
        assert_eq!(
            spell("net.http", "Client").as_deref(),
            Some("net.http.Client")
        );
        assert_eq!(spell("toml", "Value").as_deref(), Some("Toml"));
        assert_eq!(spell("toml", "Other"), None);
        assert_eq!(spell("yaml", "Value"), None);
        assert_eq!(spell("std", "Int").as_deref(), Some("Int"));
        assert_eq!(spell("std", "Str").as_deref(), Some("std.Str"));
    }
}
