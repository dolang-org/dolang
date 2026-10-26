//! The modules each configuration sees, and who imports whom.
//!
//! A configuration's [`Store`] answers what a module is right now: the client's
//! buffer when a document open in the editor is that module, else its source
//! on the module path, else its bundled typelib.  Open documents are matched to
//! modules by each store's own module path, so a document is a module of
//! every configuration whose module path contains it.  A store records each
//! module's imports, and those of the open documents checked through it, so a
//! change to a module reaches the open documents that depend on it.
//!
//! A unit doesn't depend on the modules it imports, so a change leaves its
//! dependents' units alone and only their checks need redoing.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use dolang_compile::{Unit, typeck};
use tower_lsp_server::ls_types::Uri;

use crate::{
    backend::{Import, Settings, compile_document},
    module_path::ModulePath,
    typelib_index,
};

/// The most module sources one check compiles
const MAX_MODULES: usize = 512;

/// A store's configuration directory, or `None` for configurations with no
/// module path, which all see the same modules
type Key = Option<PathBuf>;

/// Every configuration's modules, and the documents open in the editor.
#[derive(Default)]
pub(crate) struct Workspace {
    stores: HashMap<Key, Store>,
    documents: HashMap<Uri, Opened>,
    bundled: Bundled,
}

/// An open document's latest unit.
struct Opened {
    path: PathBuf,
    /// The store its checks resolve through
    store: Key,
    /// The module it is in its own store, or `None` for a script
    module: Option<String>,
    unit: Arc<Unit<'static>>,
}

/// The modules one configuration sees.
struct Store {
    modules: Option<Arc<ModulePath>>,
    /// The prelude its module sources are compiled with, which is its
    /// configuration's. A store with no module path compiles none.
    prelude: Vec<Import>,
    /// The open documents that are each module here, the most recent last
    buffers: HashMap<String, Vec<Uri>>,
    /// What each module resolved to when last looked up
    entries: HashMap<String, Entry>,
    /// What imports each module, by name, whether it resolved or not
    importers: HashMap<String, HashSet<Node>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Node {
    Module(String),
    Document(Uri),
}

struct Entry {
    origin: Origin,
    imports: Vec<String>,
}

enum Origin {
    /// A buffer or a source on the module path, compiled as the module
    Source(Arc<Unit<'static>>),
    Bundled,
    Missing,
}

/// What a module looks up to now, before it is compiled.
enum Found<'a> {
    Buffer(&'a Opened),
    File(PathBuf, String),
    Bundled,
    Missing,
}

/// What a check of a document is checked against.
pub(crate) struct Resolved<'w> {
    /// The sources of the modules it imports, directly or transitively
    pub(crate) units: Vec<(String, Arc<Unit<'static>>)>,
    /// The bundled typelibs of the rest, sorted by module
    pub(crate) typelibs: Vec<&'w typeck::Typelib<'static>>,
    /// The other open documents whose checks the lookups made stale
    pub(crate) stale: HashSet<Uri>,
}

impl Workspace {
    /// Records an open document's unit, compiled as `module` in the store of
    /// `settings`, and returns the open documents whose checks it makes stale.
    pub(crate) fn update(
        &mut self,
        uri: &Uri,
        path: &Path,
        settings: Option<&Settings>,
        module: Option<&str>,
        unit: &Arc<Unit<'static>>,
    ) -> HashSet<Uri> {
        let modules = settings.and_then(|settings| settings.modules.clone());
        let key = modules.as_ref().map(|modules| modules.base().to_owned());
        let prelude = match (settings, &modules) {
            (Some(settings), Some(_)) => settings.prelude.clone(),
            _ => Vec::new(),
        };
        let opened = Opened {
            path: path.to_owned(),
            store: key.clone(),
            module: module.map(str::to_owned),
            unit: unit.clone(),
        };
        let mut stale = HashSet::new();
        match self.documents.insert(uri.clone(), opened) {
            Some(old) if old.store == key && Arc::ptr_eq(&old.unit, unit) => return stale,
            Some(old) => {
                if let Some(store) = self.stores.get_mut(&old.store) {
                    store.unlink_all(&Node::Document(uri.clone()), imports(&old.unit));
                }
                if old.store != key {
                    self.release(&old.store);
                }
            }
            None => {}
        }
        if self
            .stores
            .get(&key)
            .is_none_or(|store| store.modules != modules || store.prelude != prelude)
        {
            // A new configuration, or one that changed: everything it saw may
            // have changed
            if let Some(old) = self.stores.remove(&key) {
                stale.extend(old.documents());
            }
            let store = Store::new(modules, prelude, &self.documents);
            self.stores.insert(key.clone(), store);
        }
        let store = self.stores.get_mut(&key).expect("just ensured");
        for import in imports(unit) {
            store.link(import, Node::Document(uri.clone()));
        }
        for (store_key, store) in &mut self.stores {
            let name = if *store_key == key {
                module.map(str::to_owned)
            } else {
                store.module_of(path)
            };
            if let Some(name) = name {
                let buffers = store.buffers.entry(name.clone()).or_default();
                buffers.retain(|other| other != uri);
                buffers.push(uri.clone());
                stale.extend(store.invalidate(&name));
            }
        }
        stale
    }

    /// Forgets a closed document, returning the open documents whose checks
    /// its buffer made stale.
    pub(crate) fn close(&mut self, uri: &Uri) -> HashSet<Uri> {
        let Some(old) = self.documents.remove(uri) else {
            return HashSet::new();
        };
        if let Some(store) = self.stores.get_mut(&old.store) {
            store.unlink_all(&Node::Document(uri.clone()), imports(&old.unit));
        }
        let mut stale = HashSet::new();
        for store in self.stores.values_mut() {
            for name in store.forget(uri) {
                stale.extend(store.invalidate(&name));
            }
        }
        self.release(&old.store);
        stale.remove(uri);
        stale
    }

    /// Notes files on disk that changed, appeared or went away, returning the
    /// open documents whose checks they make stale.
    ///
    /// A module an open document holds is the buffer, not the file, so a
    /// change to its file changes nothing.
    pub(crate) fn files(&mut self, paths: &[PathBuf]) -> HashSet<Uri> {
        let mut stale = HashSet::new();
        for store in self.stores.values_mut() {
            let Some(modules) = store.modules.clone() else {
                continue;
            };
            for path in paths {
                if let Some(name) = modules.module_at(path)
                    && !store.buffers.contains_key(&name)
                {
                    stale.extend(store.invalidate(&name));
                }
            }
        }
        stale
    }

    /// Drops a store once no open document checks through it.
    fn release(&mut self, key: &Key) {
        if !self.documents.values().any(|opened| opened.store == *key) {
            self.stores.remove(key);
        }
    }

    /// The modules an open document imports, directly or transitively, plus
    /// `proc` for the pipe types, each once, or `None` if it isn't open.
    ///
    /// The document's own module is never looked up, so it replaces its
    /// bundled copy.  An import found nowhere is left out, for the check to
    /// report as not found.
    pub(crate) fn resolve(&mut self, uri: &Uri) -> Option<Resolved<'_>> {
        let opened = self.documents.get(uri)?;
        let store = self.stores.get_mut(&opened.store)?;
        let mut queue: Vec<String> = imports(&opened.unit).map(str::to_owned).collect();
        let mut seen: HashSet<String> = opened.module.iter().cloned().collect();
        let mut units = Vec::new();
        let mut bundled = Vec::new();
        let mut stale = HashSet::new();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let found = store.lookup(&name, &self.documents, &mut self.bundled);
            if matches!(found, Found::Buffer(_) | Found::File(..)) && units.len() == MAX_MODULES {
                log::error!("a check needs more than {MAX_MODULES} module sources");
                continue;
            }
            stale.extend(store.refresh(&name, found, &self.bundled));
            let entry = &store.entries[&name];
            queue.extend(entry.imports.iter().cloned());
            match &entry.origin {
                Origin::Source(unit) => units.push((name, unit.clone())),
                Origin::Bundled => bundled.push(name),
                Origin::Missing => {}
            }
        }
        let mut typelibs: Vec<_> = (bundled.iter())
            .filter_map(|name| self.bundled.0.get(name)?.as_ref())
            .collect();
        typelibs.sort_by_key(|typelib| typelib.module());
        stale.remove(uri);
        Some(Resolved {
            units,
            typelibs,
            stale,
        })
    }
}

impl Store {
    /// A store for `modules`, with the open documents that are its modules.
    fn new(
        modules: Option<Arc<ModulePath>>,
        prelude: Vec<Import>,
        documents: &HashMap<Uri, Opened>,
    ) -> Self {
        let mut store = Self {
            modules,
            prelude,
            buffers: HashMap::new(),
            entries: HashMap::new(),
            importers: HashMap::new(),
        };
        for (uri, opened) in documents {
            if let Some(name) = store.module_of(&opened.path) {
                store.buffers.entry(name).or_default().push(uri.clone());
            }
        }
        store
    }

    fn module_of(&self, path: &Path) -> Option<String> {
        self.modules.as_ref()?.module_of(path)
    }

    /// The open documents whose checks resolve through this store.
    fn documents(&self) -> impl Iterator<Item = Uri> + '_ {
        (self.importers.values().flatten()).filter_map(|node| match node {
            Node::Document(uri) => Some(uri.clone()),
            Node::Module(_) => None,
        })
    }

    /// Removes a closed document's buffers, returning the modules they were.
    fn forget(&mut self, uri: &Uri) -> Vec<String> {
        let mut names = Vec::new();
        self.buffers.retain(|name, buffers| {
            if let Some(index) = buffers.iter().position(|other| other == uri) {
                buffers.remove(index);
                names.push(name.clone());
            }
            !buffers.is_empty()
        });
        names
    }

    /// What a module is now: the newest open buffer of it, else its source on
    /// the module path, else its bundled typelib if the module path allows.
    fn lookup<'d>(
        &self,
        name: &str,
        documents: &'d HashMap<Uri, Opened>,
        bundled: &mut Bundled,
    ) -> Found<'d> {
        let buffer = (self.buffers.get(name))
            .and_then(|buffers| buffers.last())
            .and_then(|uri| documents.get(uri));
        if let Some(opened) = buffer {
            return Found::Buffer(opened);
        }
        if let Some((path, text)) = self.modules.as_ref().and_then(|modules| modules.open(name)) {
            return Found::File(path, text);
        }
        if self.modules.as_ref().is_none_or(|modules| modules.bundled)
            && bundled.get(name).is_some()
        {
            return Found::Bundled;
        }
        Found::Missing
    }

    /// Brings a module's entry up to what it was just found to be, returning
    /// the open documents whose checks a change makes stale.
    fn refresh(&mut self, name: &str, found: Found<'_>, bundled: &Bundled) -> HashSet<Uri> {
        let current = self
            .entries
            .get(name)
            .is_some_and(|entry| match (&entry.origin, &found) {
                (Origin::Source(unit), Found::Buffer(opened)) => {
                    unit.source() == opened.unit.source()
                }
                (Origin::Source(unit), Found::File(_, text)) => unit.source() == text,
                (Origin::Bundled, Found::Bundled) | (Origin::Missing, Found::Missing) => true,
                _ => false,
            });
        if current {
            return HashSet::new();
        }
        let stale = self.invalidate(name);
        let origin = match found {
            // The document's own unit, when it is this module in its store too
            Found::Buffer(opened) if opened.module.as_deref() == Some(name) => {
                Origin::Source(opened.unit.clone())
            }
            Found::Buffer(opened) => Origin::Source(Arc::new(compile_document(
                &opened.path,
                opened.unit.source().to_owned(),
                &self.prelude,
                Some(name),
            ))),
            Found::File(path, text) => Origin::Source(Arc::new(compile_document(
                &path,
                text,
                &self.prelude,
                Some(name),
            ))),
            Found::Bundled => Origin::Bundled,
            Found::Missing => Origin::Missing,
        };
        let imports: Vec<String> = match &origin {
            Origin::Source(unit) => unit.imports().into_iter().map(str::to_owned).collect(),
            Origin::Bundled => (bundled.0.get(name))
                .and_then(Option::as_ref)
                .map(|typelib| typelib.imports().into_iter().map(str::to_owned).collect())
                .unwrap_or_default(),
            Origin::Missing => Vec::new(),
        };
        for import in &imports {
            self.link(import, Node::Module(name.to_owned()));
        }
        self.entries
            .insert(name.to_owned(), Entry { origin, imports });
        stale
    }

    /// Drops a module's entry, returning the open documents that import it,
    /// directly or transitively.
    ///
    /// The entry's own imports are unlinked with it.  That leaves nothing out:
    /// every module that reached a document through it is a dependent of this
    /// one, invalidated here too, and the next check links them again.
    fn invalidate(&mut self, name: &str) -> HashSet<Uri> {
        let mut stale = HashSet::new();
        let mut seen = HashSet::new();
        let mut queue = vec![name.to_owned()];
        while let Some(name) = queue.pop() {
            for node in self.importers.get(&name).into_iter().flatten() {
                match node {
                    Node::Module(importer) if seen.insert(importer.clone()) => {
                        queue.push(importer.clone())
                    }
                    Node::Module(_) => {}
                    Node::Document(uri) => {
                        stale.insert(uri.clone());
                    }
                }
            }
        }
        if let Some(Entry { imports, .. }) = self.entries.remove(name) {
            self.unlink_all(&Node::Module(name.to_owned()), &imports);
        }
        stale
    }

    fn link(&mut self, import: &str, node: Node) {
        self.importers
            .entry(import.to_owned())
            .or_default()
            .insert(node);
    }

    fn unlink_all(&mut self, node: &Node, imports: impl IntoIterator<Item = impl AsRef<str>>) {
        for import in imports {
            let import = import.as_ref();
            if let Some(importers) = self.importers.get_mut(import) {
                importers.remove(node);
                if importers.is_empty() {
                    self.importers.remove(import);
                }
            }
        }
    }
}

/// What a check of `unit` needs first: its imports, plus `proc` for the pipe
/// types `strand` stands for.
fn imports<'a>(unit: &'a Unit<'_>) -> impl Iterator<Item = &'a str> {
    unit.imports().into_iter().chain(["proc"])
}

/// The bundled typelibs, decoded as checks first need them, for every store.
#[derive(Default)]
struct Bundled(HashMap<String, Option<typeck::Typelib<'static>>>);

impl Bundled {
    /// A bundled module's typelib, decoded the first time a check needs it.
    fn get(&mut self, name: &str) -> Option<&typeck::Typelib<'static>> {
        (self.0.entry(name.to_owned()))
            .or_insert_with(|| {
                let bytes = typelib_index::lookup(name)?;
                typeck::Typelib::read(bytes)
                    .inspect_err(|err| log::error!("bundled typelib {name} is unreadable: {err}"))
                    .ok()
            })
            .as_ref()
    }
}
