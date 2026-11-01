//! The settings in a `.dolang-lsp.toml`.

use std::{fs, path::Path, sync::Arc};

use toml::{Table, Value as TomlValue};

use crate::module_path::ModulePath;

/// The file that configures a workspace, at its root, or a subtree below it
pub(crate) const CONFIG_FILE_NAME: &str = ".dolang-lsp.toml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Import {
    Module(String),
    ModuleAs(String, String),
    Item(String, String),
    ItemAs(String, String, String),
    TypeItem(String, String),
    TypeItemAs(String, String, String),
}

impl Import {
    fn item(module: &str, item: &str, bind: Option<&str>) -> Result<Self, String> {
        let type_only = item.starts_with('@');
        let item = if type_only { &item[1..] } else { item };
        if type_only && item.is_empty() {
            return Err("type-only prelude item name cannot be empty".into());
        }
        Ok(match (type_only, bind) {
            (false, None) => Self::Item(module.into(), item.into()),
            (false, Some(bind)) => Self::ItemAs(module.into(), item.into(), bind.into()),
            (true, None) => Self::TypeItem(module.into(), item.into()),
            (true, Some(bind)) => Self::TypeItemAs(module.into(), item.into(), bind.into()),
        })
    }
}

/// The tables a configuration sets, each `None` when it leaves the table out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) prelude: Option<Vec<Import>>,
    pub(crate) modules: Option<Arc<ModulePath>>,
}

/// Reads the configuration at `path`.  At a workspace's root, `root` is the
/// root, which the module path is relative to; below it, `None`, and a
/// `modules` table is an error.
pub(crate) fn read(path: &Path, root: Option<&Path>) -> Result<Settings, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let value = toml::from_str::<TomlValue>(&content)
        .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
    parse(&value, root)
}

#[cfg(test)]
fn parse_value(value: &TomlValue) -> Result<Settings, String> {
    parse(value, Some(Path::new(".")))
}

/// Parses settings found at `root`, or below a root when `None`.
fn parse(value: &TomlValue, root: Option<&Path>) -> Result<Settings, String> {
    let table = value
        .as_table()
        .ok_or_else(|| "settings are not a table as expected".to_owned())?;
    let mut prelude = None;
    let mut modules = None;

    for (key, value) in table {
        match key.as_str() {
            "prelude" => parse_prelude_table(value, prelude.insert(Vec::new()))?,
            "modules" => {
                let root = root.ok_or_else(|| {
                    "a modules table is allowed only in the configuration at the workspace root"
                        .to_owned()
                })?;
                modules = Some(Arc::new(parse_modules_table(value, root)?));
            }
            _ => return Err(format!("unexpected key in settings: {key}")),
        }
    }

    Ok(Settings { prelude, modules })
}

fn parse_modules_table(value: &TomlValue, dir: &Path) -> Result<ModulePath, String> {
    let table = value
        .as_table()
        .ok_or_else(|| "modules is not a table as expected".to_owned())?;
    let mut path = Vec::new();
    let mut bundled = true;

    for (key, value) in table {
        match key.as_str() {
            "path" => {
                let entries = value
                    .as_array()
                    .ok_or_else(|| "modules.path is not an array as expected".to_owned())?;
                for entry in entries {
                    let entry = entry.as_str().ok_or_else(|| {
                        "modules.path entries are not strings as expected".to_owned()
                    })?;
                    path.push(entry.to_owned());
                }
            }
            "bundled" => {
                bundled = value
                    .as_bool()
                    .ok_or_else(|| "modules.bundled is not a boolean as expected".to_owned())?;
            }
            _ => return Err(format!("unexpected key in modules: {key}")),
        }
    }

    ModulePath::load(dir, &path, bundled)
}

fn parse_prelude_table(value: &TomlValue, prelude: &mut Vec<Import>) -> Result<(), String> {
    let table = value
        .as_table()
        .ok_or_else(|| "prelude is not a table as expected".to_owned())?;

    for (module, value) in table {
        parse_prelude_entry(module, value, prelude)?;
    }

    Ok(())
}

fn parse_prelude_entry(
    module: &str,
    value: &TomlValue,
    prelude: &mut Vec<Import>,
) -> Result<(), String> {
    match value {
        TomlValue::Boolean(true) => prelude.push(Import::Module(module.to_owned())),
        TomlValue::Boolean(false) => {
            return Err(format!("prelude entry for {module} cannot be false"));
        }
        TomlValue::String(bind) => {
            prelude.push(Import::ModuleAs(module.to_owned(), bind.clone()));
        }
        TomlValue::Array(items) => {
            for item in items {
                let item = item.as_str().ok_or_else(|| {
                    format!("prelude array for {module} must contain only strings")
                })?;
                prelude.push(Import::item(module, item, None)?);
            }
        }
        TomlValue::Table(table) => parse_prelude_descriptor(module, table, prelude)?,
        _ => {
            return Err(format!(
                "prelude entry for {module} must be true, a string, an array, or a table"
            ));
        }
    }

    Ok(())
}

fn parse_prelude_descriptor(
    module: &str,
    table: &Table,
    prelude: &mut Vec<Import>,
) -> Result<(), String> {
    for (key, value) in table {
        if key == "mod" {
            match value {
                TomlValue::Boolean(true) => prelude.push(Import::Module(module.to_owned())),
                TomlValue::Boolean(false) => {
                    return Err(format!("descriptor entry {module}.mod cannot be false"));
                }
                TomlValue::String(bind) => {
                    prelude.push(Import::ModuleAs(module.to_owned(), bind.clone()));
                }
                _ => {
                    return Err(format!(
                        "descriptor entry {module}.mod must be true or a string"
                    ));
                }
            }
            continue;
        }

        match value {
            TomlValue::Boolean(true) => {
                prelude.push(Import::item(module, key, None)?);
            }
            TomlValue::Boolean(false) => {
                return Err(format!("descriptor entry {module}.{key} cannot be false"));
            }
            TomlValue::String(bind) => {
                prelude.push(Import::item(module, key, Some(bind))?);
            }
            _ => {
                return Err(format!(
                    "descriptor entry {module}.{key} must be true or a string"
                ));
            }
        }
    }

    Ok(())
}

/// The prelude where no configuration sets one: the shell's.
pub(crate) fn default_prelude() -> Vec<Import> {
    vec![
        Import::Module("shell".into()),
        Import::Item("proc".into(), "run".into()),
        Import::Item("shell".into(), "cd".into()),
        Import::Item("shell".into(), "env".into()),
        Import::Item("shell".into(), "exit".into()),
        Import::Item("shell".into(), "sub".into()),
        Import::Item("term".into(), "echo".into()),
        Import::Item("term".into(), "print".into()),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::Kind;

    use super::*;
    use crate::{backend::compile_document, module_path::tests::Directory};

    fn parse_toml(input: &str) -> TomlValue {
        toml::from_str(input).unwrap()
    }

    #[test]
    fn parse_module_import() {
        let settings = parse_value(&parse_toml("[prelude]\nshell = true\n")).unwrap();

        assert_eq!(
            settings.prelude,
            Some(vec![Import::Module("shell".to_owned())])
        );
    }

    #[test]
    fn parse_item_alias() {
        let settings = parse_value(&parse_toml("[prelude.proc]\nrun = true\n")).unwrap();

        assert_eq!(
            settings.prelude.unwrap(),
            vec![Import::Item("proc".to_owned(), "run".to_owned())]
        );
    }

    #[test]
    fn parse_item_array() {
        let settings = parse_value(&parse_toml(
            "[prelude]\nregression = [\"assert\", \"log\"]\n",
        ))
        .unwrap();

        assert_eq!(
            settings.prelude.unwrap(),
            vec![
                Import::Item("regression".to_owned(), "assert".to_owned()),
                Import::Item("regression".to_owned(), "log".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_descriptor_table_with_mod_and_items() {
        let settings =
            parse_value(&parse_toml("[prelude.proc]\nmod = true\nsub = true\n")).unwrap();

        assert_eq!(
            settings.prelude.unwrap(),
            vec![
                Import::Module("proc".to_owned()),
                Import::Item("proc".to_owned(), "sub".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_type_only_prelude_items() {
        let settings = parse_value(&parse_toml(
            "[prelude]\nstd = [\"@Never\"]\n[prelude.custom]\n\"@Seq\" = \"Sequence\"\n\"@Index\" = true\n",
        )).unwrap();
        let prelude = settings.prelude.unwrap();
        assert!(prelude.contains(&Import::TypeItem("std".into(), "Never".into())));
        assert!(prelude.contains(&Import::TypeItem("custom".into(), "Index".into())));
        assert!(prelude.contains(&Import::TypeItemAs(
            "custom".into(),
            "Seq".into(),
            "Sequence".into()
        )));
        for config in ["[prelude]\nstd = [\"@\"]", "[prelude.std]\n\"@\" = \"Bad\""] {
            assert!(
                parse_value(&parse_toml(config))
                    .unwrap_err()
                    .contains("cannot be empty")
            );
        }
    }

    #[test]
    fn configured_type_only_items_resolve_to_prelude_nodes() {
        for config in [
            "[prelude]\nstd = [\"@Seq\"]",
            "[prelude.std]\n\"@Seq\" = \"Sequence\"",
        ] {
            let settings = parse_value(&parse_toml(config)).unwrap();
            let name = if config.contains("Sequence") {
                "Sequence"
            } else {
                "Seq"
            };
            let unit = compile_document(
                Path::new("m.dol"),
                format!("def first items@{name}[Int] -> Int\n  items[0]\nfirst [1]\n"),
                settings.prelude.as_deref().unwrap(),
                None,
            );
            assert!(unit.diagnostics().next().is_none());
            assert!(
                unit.nodes()
                    .any(|(_, node)| matches!(node.kind(), Kind::PreludeItem {
                module: "std", item: "Seq", name: bind, type_only: true,
            } if bind == name))
            );
        }
    }

    #[test]
    fn reject_unknown_top_level_key() {
        let error = parse_value(&parse_toml("other = true\n")).unwrap_err();

        assert!(error.contains("unexpected key in settings"));
    }

    #[test]
    fn reject_false_module_value() {
        let error = parse_value(&parse_toml("[prelude]\nshell = false\n")).unwrap_err();

        assert!(error.contains("cannot be false"));
    }

    #[test]
    fn reject_non_string_array_item() {
        let error = parse_value(&parse_toml("[prelude]\nshell = [\"echo\", 1]\n")).unwrap_err();

        assert!(error.contains("must contain only strings"));
    }

    #[test]
    fn reject_invalid_descriptor_value() {
        let error = parse_value(&parse_toml("[prelude.shell]\necho = 1\n")).unwrap_err();

        assert!(error.contains("must be true or a string"));
    }

    #[test]
    fn missing_tables_are_left_out() {
        let settings = parse_value(&parse_toml("")).unwrap();
        assert_eq!(
            settings,
            Settings {
                prelude: None,
                modules: None
            }
        );
        let settings = parse_value(&parse_toml("[prelude]\n")).unwrap();
        assert_eq!(settings.prelude, Some(vec![]));
    }

    #[test]
    fn parse_modules() {
        let tree = Directory::new();
        tree.write("lib/a.dol", "");
        let value = parse_toml("[modules]\npath = [\"lib\", \"/abs\", \"../up\"]\n");
        let settings = parse(&value, Some(&tree.0)).unwrap();
        // The rejected entries are logged; the rest applies
        let modules = settings.modules.unwrap();
        assert!(modules.bundled);
        assert_eq!(
            modules.module_of(&tree.0.join("lib/a.dol")).as_deref(),
            Some("a")
        );

        let value = parse_toml("[modules]\nbundled = false\n");
        let settings = parse(&value, Some(&tree.0)).unwrap();
        assert!(!settings.modules.unwrap().bundled);
    }

    #[test]
    fn reject_modules_below_the_root() {
        let value = parse_toml("[prelude]\n[modules]\npath = [\"lib\"]\n");
        let error = parse(&value, None).unwrap_err();
        assert!(error.contains("only in the configuration at the workspace root"));
        let settings = parse(&parse_toml("[prelude]\n"), None).unwrap();
        assert_eq!(settings.prelude, Some(vec![]));
    }

    #[test]
    fn reject_invalid_modules() {
        for (config, expected) in [
            ("modules = 1", "not a table"),
            ("[modules]\nother = 1", "unexpected key in modules: other"),
            ("[modules]\npath = \"lib\"", "not an array"),
            ("[modules]\npath = [1]", "not strings"),
            ("[modules]\nbundled = \"no\"", "not a boolean"),
        ] {
            let error = parse_value(&parse_toml(config)).unwrap_err();
            assert!(error.contains(expected), "{config}: {error}");
        }
    }
}
