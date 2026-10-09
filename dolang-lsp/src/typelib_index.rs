//! Bundled typelibs, looked up by module name without loading source or extensions.

pub(crate) fn lookup(name: &str) -> Option<&'static [u8]> {
    lookup_in(ENTRIES, name)
}

fn lookup_in<'a>(entries: &[(&str, &'a [u8])], name: &str) -> Option<&'a [u8]> {
    entries
        .binary_search_by_key(&name, |(module, _)| *module)
        .ok()
        .map(|index| entries[index].1)
}

include!(concat!(env!("OUT_DIR"), "/typelib_index_data.rs"));

#[cfg(test)]
use crate::doc_index::typelib_build;

#[cfg(test)]
mod tests {
    use super::*;
    use dolang_compile::{Config, Mode, typeck};
    use std::{fs, path::PathBuf};

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dolang-lsp-typelibs-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, module: &str) {
            let mut config = Config::new();
            config.mode(Mode::Module {
                name: module.into(),
            });
            config.typecheck(true);
            let path = PathBuf::from("fixture.dol");
            let unit = config.unit(&path, "pub let value = 1\n");
            fs::write(
                self.0.join(format!("{name}.dolt")),
                typeck::typelib(&unit).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn lookup_handles_dotted_and_unknown_modules() {
        let entries = [("a", &b"first"[..]), ("a.b", &b"nested"[..])];
        assert_eq!(lookup_in(&entries, "a.b"), Some(&b"nested"[..]));
        assert_eq!(lookup_in(&entries, "missing"), None);
        assert_eq!(lookup_in(&[], "a"), None);
    }

    #[test]
    fn generation_sorts_tracks_current_files_and_uses_absolute_paths() {
        let dir = Directory::new();
        dir.write("z", "z");
        dir.write("a.b", "a.b");
        fs::write(dir.0.join("ignored.json"), "{}").unwrap();
        let table = typelib_build::render(Some(&dir.0));
        assert!(table.find("\"a.b\"").unwrap() < table.find("\"z\"").unwrap());
        assert!(table.contains(&format!(
            "{:?}",
            dir.0.canonicalize().unwrap().join("a.b.dolt")
        )));
        assert!(!table.contains("ignored"));
        fs::remove_file(dir.0.join("z.dolt")).unwrap();
        dir.write("new", "new");
        let table = typelib_build::render(Some(&dir.0));
        assert!(!table.contains("\"z\""));
        assert!(table.contains("\"new\""));
        assert_eq!(
            typelib_build::render(None),
            "pub(crate) static ENTRIES: &[(&str, &[u8])] = &[\n];\n"
        );
    }

    #[test]
    #[should_panic(expected = "failed to decode")]
    fn rejects_malformed_blob() {
        let dir = Directory::new();
        fs::write(dir.0.join("bad.dolt"), b"invalid").unwrap();
        typelib_build::render(Some(&dir.0));
    }

    #[test]
    #[should_panic(expected = "module mismatch")]
    fn rejects_mismatched_module() {
        let dir = Directory::new();
        dir.write("a", "b");
        typelib_build::render(Some(&dir.0));
    }

    #[test]
    #[should_panic(expected = "failed to read DOLANG_LSP_TYPELIB_DIR")]
    fn rejects_missing_directory() {
        let dir = Directory::new();
        typelib_build::render(Some(&dir.0.join("missing")));
    }

    #[test]
    fn embedded_bundle_decodes_and_checks_together() {
        // Ordinary builds have an empty index; populated builds exercise the real bundle.
        if ENTRIES.is_empty() {
            return;
        }
        for name in ["std", "strand", "json", "proc", "args"] {
            assert!(lookup(name).is_some(), "missing {name}");
        }
        let typelibs: Vec<_> = ENTRIES
            .iter()
            .map(|(name, bytes)| {
                let typelib = typeck::Typelib::read(bytes).unwrap();
                assert_eq!(*name, typelib.module());
                typelib
            })
            .collect();
        let mut builder = typeck::Builder::new();
        builder.pipes(("proc", "PipeSender"), ("proc", "PipeReceiver"));
        for typelib in &typelibs {
            builder.typelib(typelib).unwrap();
        }
        let check = builder.check();
        let errors: Vec<_> = check
            .diagnostics()
            .filter(|diag| diag.severity() == dolang_compile::diag::Severity::Error)
            .map(|diag| diag.message().to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
