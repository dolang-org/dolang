//! The module path a configuration declares, and the containment of what it
//! has the server read.
//!
//! A configuration comes from whatever tree the editor opens, so everything it
//! directs the server to read stays inside the configuration's directory.  Each
//! root is canonicalized and must lie under that directory when the
//! configuration loads, and again whenever a module is opened under it; each
//! module file must lie under its root's canonical path and be a regular file.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

/// The most roots a configuration's wildcards may expand to
const MAX_ROOTS: usize = 256;
/// The largest module source read from a root
const MAX_MODULE_BYTES: u64 = 4 << 20;

/// When a file was last modified, and its size: what tells a change to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// The directories searched for modules, in order, all under `base`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModulePath {
    /// The configuration's canonical directory
    base: PathBuf,
    /// Canonical, each once
    roots: Vec<PathBuf>,
    /// Whether a module no root has is found among the bundled typelibs
    pub(crate) bundled: bool,
}

impl ModulePath {
    /// The module path of a configuration in `dir` listing `entries`.  An
    /// entry that names no root, or one outside `dir`, is logged and skipped.
    pub(crate) fn load(dir: &Path, entries: &[String], bundled: bool) -> Result<Self, String> {
        let base = dir
            .canonicalize()
            .map_err(|err| format!("cannot resolve {}: {err}", dir.display()))?;
        let mut roots = Vec::new();
        for entry in entries {
            if let Err(err) = expand(&base, entry, &mut roots) {
                log::error!(
                    "module path entry `{entry}` in {} rejected: {err}",
                    dir.display()
                );
            }
        }
        Ok(Self {
            base,
            roots,
            bundled,
        })
    }

    /// The module path of a workspace at `root` that configures none: its
    /// `lib/`, then the bundled modules.  `lib/` need not exist yet, since a
    /// root is resolved again whenever a module is looked for under it.
    pub(crate) fn convention(root: &Path) -> Result<Self, String> {
        let base = root
            .canonicalize()
            .map_err(|err| format!("cannot resolve {}: {err}", root.display()))?;
        Ok(Self {
            roots: vec![base.join("lib")],
            base,
            bundled: true,
        })
    }

    /// The module a document at `path` is, if it is a source under a root.
    pub(crate) fn module_of(&self, path: &Path) -> Option<String> {
        if path.extension().is_none_or(|ext| ext != "dol") {
            return None;
        }
        self.name_of(&path.canonicalize().ok()?)
    }

    /// The module a canonical path under a root names.
    fn name_of(&self, path: &Path) -> Option<String> {
        let rest = self
            .roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())?;
        let mut parts: Vec<&str> = rest
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<_>>()?;
        let last = parts.pop()?;
        if last != "mod.dol" {
            parts.push(last.strip_suffix(".dol")?);
        }
        (!parts.is_empty() && parts.iter().all(|part| is_name(part))).then(|| parts.join("."))
    }

    /// The path, stamp and text of a module's source, from the first root
    /// that has it as `a/b.dol` or `a/b/mod.dol`.  A candidate that escapes
    /// its root, isn't a regular file or is too large is logged and passed
    /// over.
    pub(crate) fn open(&self, module: &str) -> Option<(PathBuf, Stamp, String)> {
        self.find(module, read_contained)
    }

    /// The path and stamp of the source `open` would read, without reading it.
    pub(crate) fn locate(&self, module: &str) -> Option<(PathBuf, Stamp)> {
        self.find(module, stat_contained)
    }

    /// What `found` makes of the first candidate for a module's source that
    /// exists.
    fn find<T>(
        &self,
        module: &str,
        found: impl Fn(&Path, &Path) -> Result<Option<T>, String>,
    ) -> Option<T> {
        let parts: Vec<&str> = module.split('.').collect();
        if !parts.iter().all(|part| is_name(part)) {
            log::error!("module name `{module}` can't name a file");
            return None;
        }
        let relative: PathBuf = parts.iter().collect();
        for root in &self.roots {
            // The root may have been replaced since the configuration loaded
            let root = match root.canonicalize() {
                Ok(canonical) if canonical.starts_with(&self.base) => canonical,
                Ok(canonical) => {
                    log::error!(
                        "module root {} now resolves outside {}",
                        canonical.display(),
                        self.base.display()
                    );
                    continue;
                }
                Err(_) => continue,
            };
            let candidates = [
                root.join(&relative).with_extension("dol"),
                root.join(&relative).join("mod.dol"),
            ];
            for candidate in candidates {
                match found(&root, &candidate) {
                    Ok(Some(found)) => return Some(found),
                    Ok(None) => {}
                    Err(err) => {
                        log::error!("module source {} rejected: {err}", candidate.display());
                    }
                }
            }
        }
        None
    }
}

/// Adds the canonical roots `entry` names under `base` to `roots`.
fn expand(base: &Path, entry: &str, roots: &mut Vec<PathBuf>) -> Result<(), String> {
    let entry = Path::new(entry);
    if entry.has_root() {
        return Err("not relative to the configuration's directory".into());
    }
    let mut found = vec![base.to_owned()];
    for component in entry.components() {
        let part = match component {
            Component::CurDir => continue,
            Component::Normal(part) => part,
            Component::ParentDir => return Err("`..` is not allowed".into()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("not relative to the configuration's directory".into());
            }
        };
        let Some(pattern) = part.to_str().filter(|part| part.contains('*')) else {
            for dir in &mut found {
                dir.push(part);
            }
            continue;
        };
        let mut matched = Vec::new();
        for dir in found {
            // List only directories inside the base
            let Ok(dir) = contained(base, &dir) else {
                continue;
            };
            let Ok(listing) = fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<_> = listing
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| wildcard(pattern, name))
                .collect();
            names.sort();
            for name in names {
                if matched.len() == MAX_ROOTS {
                    return Err(format!("expands to more than {MAX_ROOTS} directories"));
                }
                matched.push(dir.join(name));
            }
        }
        found = matched;
    }
    let mut kept = false;
    for dir in found {
        // A wildcard passes over what isn't a directory; a plain path names one
        let Ok(dir) = contained(base, &dir) else {
            continue;
        };
        if !dir.is_dir() {
            continue;
        }
        kept = true;
        if !roots.contains(&dir) {
            if roots.len() == MAX_ROOTS {
                return Err(format!("more than {MAX_ROOTS} roots"));
            }
            roots.push(dir);
        }
    }
    if kept {
        Ok(())
    } else {
        Err("names no directory inside the configuration's directory".into())
    }
}

/// The canonical path of `path`, which must lie under `base`.
fn contained(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|err| err.to_string())?;
    if canonical.starts_with(base) {
        Ok(canonical)
    } else {
        Err(format!("resolves outside {}", base.display()))
    }
}

/// The canonical path and stamp of `path` if it exists, as a regular file
/// under `root`.
fn stat_contained(root: &Path, path: &Path) -> Result<Option<(PathBuf, Stamp)>, String> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let canonical = contained(root, path)?;
    match fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => Ok(Some((canonical, Stamp::of(&meta)))),
        _ => Err("not a regular file".into()),
    }
}

/// Reads `path` if it exists, as a regular file under `root`.
fn read_contained(root: &Path, path: &Path) -> Result<Option<(PathBuf, Stamp, String)>, String> {
    let Some((canonical, _)) = stat_contained(root, path)? else {
        return Ok(None);
    };
    // Opening a FIFO or device could block or read without end
    let file = open_nonblocking(&canonical).map_err(|err| err.to_string())?;
    // It may have been swapped since it was resolved
    let stamp = match file.metadata() {
        Ok(meta) if meta.is_file() => Stamp::of(&meta),
        _ => return Err("not a regular file".into()),
    };
    let mut text = String::new();
    file.take(MAX_MODULE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| err.to_string())?;
    if text.len() as u64 > MAX_MODULE_BYTES {
        return Err(format!("larger than {MAX_MODULE_BYTES} bytes"));
    }
    Ok(Some((canonical, stamp, text)))
}

fn open_nonblocking(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(path)
}

/// Whether a module path segment is a plain name.
fn is_name(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters
/// and a leading `.` must be matched explicitly.
fn wildcard(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A scratch directory, removed when dropped.
    pub(crate) struct Directory(pub(crate) PathBuf);

    impl Directory {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dolang-lsp-modules-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        /// A scratch repository: a directory with a `.git`, which makes it
        /// a workspace's root.
        pub(crate) fn repo() -> Self {
            let tree = Self::new();
            tree.dir(".git");
            tree
        }

        pub(crate) fn write(&self, path: &str, text: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        pub(crate) fn dir(&self, path: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// The path and text of a module's source.
    fn opened(path: &ModulePath, module: &str) -> Option<(PathBuf, String)> {
        path.open(module).map(|(path, _, text)| (path, text))
    }

    fn load(dir: &Path, entries: &[&str]) -> ModulePath {
        let entries: Vec<String> = entries.iter().map(|&entry| entry.into()).collect();
        ModulePath::load(dir, &entries, true).unwrap()
    }

    #[test]
    fn wildcards_match_within_a_name() {
        assert!(wildcard("*", "stub"));
        assert!(wildcard("dolang-*", "dolang-ext-json"));
        assert!(wildcard("*-ext-*n", "dolang-ext-json"));
        assert!(wildcard("a*b*c", "abc"));
        assert!(!wildcard("a*b*c", "acb"));
        assert!(!wildcard("*", ".git"));
        assert!(wildcard(".*", ".git"));
        assert!(!wildcard("stub", "stubs"));
    }

    #[test]
    fn roots_expand_in_order_and_once() {
        let tree = Directory::new();
        let b = tree.dir("b/stub");
        let a = tree.dir("a/stub");
        tree.dir("c/other");
        tree.write("d/stub", "not a directory");
        let lib = tree.dir("lib");
        let path = load(&tree.0, &["lib", "*/stub", "./a/stub"]);
        assert_eq!(path.roots, [lib, a, b]);
    }

    #[test]
    fn modules_are_named_by_their_path_under_a_root() {
        let tree = Directory::new();
        tree.dir("lib");
        let path = load(&tree.0, &["lib"]);
        let flat = tree.write("lib/a.dol", "");
        let nested = tree.write("lib/a/b.dol", "");
        let package = tree.write("lib/c/mod.dol", "");
        let outside = tree.write("x.dol", "");
        let odd = tree.write("lib/a-b.dol", "");
        assert_eq!(path.module_of(&flat).as_deref(), Some("a"));
        assert_eq!(path.module_of(&nested).as_deref(), Some("a.b"));
        assert_eq!(path.module_of(&package).as_deref(), Some("c"));
        assert_eq!(path.module_of(&outside), None);
        assert_eq!(path.module_of(&odd), None);

        assert_eq!(opened(&path, "a.b"), Some((nested, String::new())));
        assert_eq!(opened(&path, "c"), Some((package, String::new())));
        assert_eq!(path.open("missing"), None);
    }

    #[test]
    fn the_convention_is_lib_even_before_it_exists() {
        let tree = Directory::new();
        let path = ModulePath::convention(&tree.0).unwrap();
        assert!(path.bundled);
        assert_eq!(path.open("a"), None);
        let a = tree.write("lib/a.dol", "");
        assert_eq!(path.module_of(&a).as_deref(), Some("a"));
        assert_eq!(opened(&path, "a"), Some((a, String::new())));
        assert_eq!(path.module_of(&tree.write("a.dol", "")), None);
    }

    #[test]
    fn earlier_roots_win() {
        let tree = Directory::new();
        let first = tree.write("one/m.dol", "1");
        tree.write("two/m.dol", "2");
        let path = load(&tree.0, &["one", "two"]);
        assert_eq!(opened(&path, "m"), Some((first, "1".into())));
    }

    #[test]
    fn absolute_and_parent_entries_are_rejected() {
        let tree = Directory::new();
        let inner = tree.dir("inner");
        tree.dir("lib");
        let absolute = tree.0.join("lib").display().to_string();
        let path = load(&inner, &[&absolute, "../lib", "x/../../lib"]);
        assert_eq!(path.roots, Vec::<PathBuf>::new());
    }

    #[test]
    fn non_directory_root_is_rejected() {
        let tree = Directory::new();
        tree.write("file", "");
        let path = load(&tree.0, &["file", "missing"]);
        assert_eq!(path.roots, Vec::<PathBuf>::new());
    }

    #[test]
    fn oversized_module_is_rejected() {
        let tree = Directory::new();
        let big = "#".repeat(MAX_MODULE_BYTES as usize + 1);
        tree.write("lib/big.dol", &big);
        let path = load(&tree.0, &["lib"]);
        assert_eq!(path.open("big"), None);
    }

    #[test]
    fn module_names_stay_names() {
        let tree = Directory::new();
        tree.write("lib/a.dol", "");
        let path = load(&tree.0, &["lib"]);
        for name in ["", "a.", "..a", "a/b", "a\\b"] {
            assert_eq!(path.open(name), None, "{name:?}");
        }
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::symlink;

        use super::*;

        #[test]
        fn symlinked_root_outside_is_rejected() {
            let outside = Directory::new();
            outside.write("m.dol", "");
            let tree = Directory::new();
            symlink(&outside.0, tree.0.join("lib")).unwrap();
            tree.dir("pkg");
            symlink(&outside.0, tree.0.join("pkg/stub")).unwrap();
            let path = load(&tree.0, &["lib", "*/stub"]);
            assert_eq!(path.roots, Vec::<PathBuf>::new());
        }

        #[test]
        fn symlinked_root_inside_is_kept() {
            let tree = Directory::new();
            let real = tree.dir("real");
            symlink(&real, tree.0.join("lib")).unwrap();
            let path = load(&tree.0, &["lib"]);
            assert_eq!(path.roots, [real]);
        }

        #[test]
        fn wildcard_does_not_list_outside() {
            let outside = Directory::new();
            outside.dir("x/stub");
            let tree = Directory::new();
            symlink(&outside.0, tree.0.join("link")).unwrap();
            let path = load(&tree.0, &["link/*/stub"]);
            assert_eq!(path.roots, Vec::<PathBuf>::new());
        }

        #[test]
        fn symlinked_module_outside_its_root_is_rejected() {
            let outside = Directory::new();
            let target = outside.write("m.dol", "secret");
            let tree = Directory::new();
            // Even a file elsewhere in the configuration's directory
            let sibling = tree.write("other/n.dol", "sibling");
            tree.dir("lib");
            symlink(&target, tree.0.join("lib/m.dol")).unwrap();
            symlink(&sibling, tree.0.join("lib/n.dol")).unwrap();
            let path = load(&tree.0, &["lib"]);
            assert_eq!(path.open("m"), None);
            assert_eq!(path.open("n"), None);
        }

        #[test]
        fn symlink_swapped_in_after_load_is_rejected() {
            let outside = Directory::new();
            outside.write("m.dol", "secret");
            let tree = Directory::new();
            tree.write("lib/m.dol", "inside");
            let path = load(&tree.0, &["lib"]);
            assert!(path.open("m").is_some());

            fs::remove_dir_all(tree.0.join("lib")).unwrap();
            symlink(&outside.0, tree.0.join("lib")).unwrap();
            assert_eq!(path.open("m"), None);

            fs::remove_file(tree.0.join("lib")).unwrap();
            tree.dir("lib");
            symlink(outside.0.join("m.dol"), tree.0.join("lib/m.dol")).unwrap();
            assert_eq!(path.open("m"), None);
        }

        #[test]
        fn fifo_module_is_rejected() {
            let tree = Directory::new();
            tree.dir("lib");
            let fifo = tree.0.join("lib/m.dol");
            let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: the path is a valid NUL-terminated string
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            let path = load(&tree.0, &["lib"]);
            assert_eq!(path.open("m"), None);
        }
    }
}
