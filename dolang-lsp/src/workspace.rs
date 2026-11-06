//! Workspaces, and the documents open in each.
//!
//! A workspace is a root, the nearest directory above a document with a
//! `doze.toml` or a `.git`, or else the document's own directory, with its
//! settings: those of
//! a `.dolang-lsp.toml` at the root, else the convention, `lib/` as the module
//! path and the shell's prelude.  A configuration below the root sets the
//! prelude of its subtree.  Its members are the documents in it that are open for any
//! reason: in the client, or as a module a check imports.  A member is one
//! revision of its document, which new text replaces: the client's newer
//! version of a document it has open, or a change to a file on disk.  Whoever
//! still holds an older revision keeps it, and finds it is no longer current.
//!
//! A member's check records what each module in its closure looked up to, and
//! is valid while every one of them still looks up the same.  Nothing else
//! invalidates it, so a change to the table needs no bookkeeping beyond the
//! change itself.  A check of the workspace checks together the members whose
//! checks aren't valid, and the closure of their imports.
//!
//! A check of a module writes its typelib, which is part of the check and as
//! valid as it is.  A later check takes a member with a valid typelib as that
//! typelib, not its source.  A module whose source fails to compile keeps the
//! typelib of the last revision that had one, its last good, which its
//! importers are checked against until its source compiles again.
//!
//! A buffer the client has open with no file of its own, an `untitled:`
//! document, is a script in the workspace of the client's first folder, else
//! in the detached workspace, whose module path is the bundled modules alone.
//! It is no module, so nothing else imports it.
//!
//! The workspace's lock guards only reading and changing its table: files are
//! read, and members compiled and checked, outside it.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};

use dolang_compile::{Unit, typeck};
use tower_lsp_server::ls_types::Uri;

use crate::{
    backend::{Checked, compile_document},
    module_path::{ModulePath, Stamp},
    settings::{self, CONFIG_FILE_NAME, Import, PROJECT_FILE_NAME},
    typelib_index,
};

/// The most module sources one check reads
const MAX_MODULES: usize = 512;

/// The types `strand`'s pipe placeholders stand for: the shell's pipelines
/// connect stages with `proc`'s pipes
pub(crate) const PIPES: [(&str, &str); 2] = [("proc", "PipeSender"), ("proc", "PipeReceiver")];

/// The live workspaces, by root, and the detached workspace.
#[derive(Default)]
pub(crate) struct Workspaces {
    table: HashMap<PathBuf, Weak<Workspace>>,
    detached: Weak<Workspace>,
    /// The client's first folder, where buffers with no file resolve imports
    folder: Option<PathBuf>,
    bundled: Arc<Bundled>,
}

/// What a workspace's table knows a document by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    /// A file, whether the client has it open or a check loaded it
    File(PathBuf),
    /// A buffer the client has open with no file, by its URI
    Untitled(Uri),
}

impl Key {
    /// The path a document is compiled as: its file's, else a relative path
    /// named for the last segment of its URI, which lies under no root.
    fn path(&self) -> PathBuf {
        match self {
            Self::File(path) => path.clone(),
            Self::Untitled(uri) => {
                let path = uri.path();
                let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
                let name = name.decode().to_string_lossy();
                match name.is_empty() {
                    true => PathBuf::from("untitled"),
                    false => PathBuf::from(&*name),
                }
            }
        }
    }
}

impl Workspaces {
    /// Sets the client's first folder, for buffers opened from now on.
    pub(crate) fn set_folder(&mut self, folder: Option<PathBuf>) {
        self.folder = folder;
    }

    /// The workspace of the document `key`: a file's own, else that of the
    /// client's first folder, else the detached workspace.
    pub(crate) fn of(&mut self, key: &Key) -> Arc<Workspace> {
        match (key, self.folder.clone()) {
            (Key::File(path), _) => self.open(path),
            (Key::Untitled(_), Some(folder)) => self.open_dir(&folder),
            (Key::Untitled(_), None) => self.detached(),
        }
    }

    /// The workspace of the document at `path`, created unless one is live.
    pub(crate) fn open(&mut self, path: &Path) -> Arc<Workspace> {
        self.open_dir(path.parent().unwrap_or(path))
    }

    /// The workspace of the documents in `dir`, created unless one is live.
    pub(crate) fn open_dir(&mut self, dir: &Path) -> Arc<Workspace> {
        let root = root(dir);
        if let Some(workspace) = self.table.get(&root).and_then(Weak::upgrade) {
            return workspace;
        }
        let config = root.join(CONFIG_FILE_NAME);
        let settings = match config.is_file() {
            true => settings::read(&config, Some(&root))
                .inspect_err(|err| {
                    log::error!("failed to load configuration {}: {err}", config.display())
                })
                .ok(),
            false => Some(settings::Settings {
                prelude: None,
                modules: None,
            }),
        };
        // An unreadable configuration leaves the workspace with nothing
        let (prelude, modules) = match settings {
            Some(settings) => {
                let modules = settings.modules.or_else(|| {
                    ModulePath::convention(&root)
                        .inspect_err(|err| log::error!("no module path: {err}"))
                        .ok()
                        .map(Arc::new)
                });
                let prelude = settings.prelude.unwrap_or_else(settings::default_prelude);
                (prelude.into(), modules)
            }
            None => (Arc::default(), None),
        };
        let workspace = Arc::new(Workspace {
            root: root.clone(),
            prelude,
            modules,
            preludes: Mutex::default(),
            bundled: self.bundled.clone(),
            members: Mutex::default(),
        });
        self.table.retain(|_, live| live.strong_count() > 0);
        self.table.insert(root, Arc::downgrade(&workspace));
        workspace
    }

    /// The workspace of buffers with no file and no folder to resolve their
    /// imports in, created unless it is live: the default prelude, and only
    /// the bundled modules.
    pub(crate) fn detached(&mut self) -> Arc<Workspace> {
        if let Some(workspace) = self.detached.upgrade() {
            return workspace;
        }
        let workspace = Arc::new(Workspace {
            root: PathBuf::new(),
            prelude: settings::default_prelude().into(),
            modules: None,
            preludes: Mutex::default(),
            bundled: self.bundled.clone(),
            members: Mutex::default(),
        });
        self.detached = Arc::downgrade(&workspace);
        workspace
    }

    /// Whether `workspace` is still the one its root opens, or the detached
    /// workspace, which no configuration affects.
    pub(crate) fn current(&self, workspace: &Arc<Workspace>) -> bool {
        self.detached.as_ptr() == Arc::as_ptr(workspace)
            || self
                .table
                .get(&workspace.root)
                .is_some_and(|live| live.as_ptr() == Arc::as_ptr(workspace))
    }

    /// Forgets the workspaces a change to the configuration in `dir` may
    /// affect: those whose roots contain it.  Their documents find their
    /// workspaces again when they next open one.
    pub(crate) fn retire(&mut self, dir: &Path) {
        self.table.retain(|root, _| !dir.starts_with(root));
    }

    /// The live workspaces.
    pub(crate) fn live(&self) -> impl Iterator<Item = Arc<Workspace>> + '_ {
        self.table
            .values()
            .chain([&self.detached])
            .filter_map(Weak::upgrade)
    }
}

/// The workspace root of the documents in `dir`: the nearest of it and its
/// ancestors that marks one, else `dir`.
fn root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|dir| marks_root(dir))
        .unwrap_or(dir)
        .to_owned()
}

/// Whether `dir` is a workspace root: a project's, with a `doze.toml`, or a
/// repository's, with a `.git` directory, or a submodule's or worktree's
/// `.git` file.
fn marks_root(dir: &Path) -> bool {
    dir.join(PROJECT_FILE_NAME).is_file() || fs::symlink_metadata(dir.join(".git")).is_ok()
}

/// One root, and its members.
pub(crate) struct Workspace {
    /// Empty for the detached workspace
    root: PathBuf,
    /// The prelude at the root
    prelude: Arc<[Import]>,
    /// `None` when the root's configuration is unreadable, and for the
    /// detached workspace
    modules: Option<Arc<ModulePath>>,
    /// The prelude of each directory below the root looked at so far
    preludes: Mutex<HashMap<PathBuf, Arc<[Import]>>>,
    bundled: Arc<Bundled>,
    /// The current revision of each member
    members: Mutex<HashMap<Key, Arc<Member>>>,
}

/// One revision of a document in a workspace.
pub(crate) struct Member {
    key: Key,
    /// The path it is compiled as
    path: PathBuf,
    /// The module it is on the module path, or `None` for a script
    module: Option<String>,
    source: Source,
    /// Compiled when first needed, which for an open document is on the
    /// worker rather than in the handler that made it
    unit: OnceLock<Arc<Unit<'static>>>,
    /// The modules a check of its unit needs, once a check has gathered it,
    /// or `None` if the checker can't take its unit
    depends: OnceLock<Option<Vec<String>>>,
    results: Mutex<Option<Results>>,
    /// The closure of the latest check of an earlier revision, which keeps
    /// its modules loaded until this one is checked
    earlier: Option<Deps>,
    /// The typelib of the latest earlier revision that had one, which stands
    /// in for its module while its unit fails to compile
    last_good: Option<Arc<Written>>,
}

/// Where a member's text comes from, and which of its texts it is.
enum Source {
    /// Open in the client, as its `version`
    Open {
        uri: Uri,
        version: i32,
        text: String,
    },
    /// Read from disk, when its file was as `stamp`
    Loaded { stamp: Stamp },
}

/// The modules a member imports, directly or transitively, and what each
/// looked up to.
type Deps = Arc<[(String, Resolution)]>;

/// A member's check, and what it was checked against.
#[derive(Clone)]
struct Results {
    checked: Checked,
    deps: Deps,
    /// The typelib the check wrote of it, if it is a module the check could
    /// write one for
    typelib: Option<Arc<Written>>,
}

/// The typelib a check wrote of each of its members it could write one for.
pub(crate) type Typelibs = Vec<Option<Box<[u8]>>>;

/// A workspace module's typelib, as a check of it wrote it, and the modules a
/// check of it needs.
pub(crate) struct Written {
    bytes: Box<[u8]>,
    depends: Vec<String>,
}

/// What a module name looks up to in a workspace.
#[derive(Clone)]
enum Resolution {
    Member(Weak<Member>),
    Bundled,
    Missing,
}

impl Resolution {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Member(a), Self::Member(b)) => Weak::ptr_eq(a, b),
            (Self::Bundled, Self::Bundled) | (Self::Missing, Self::Missing) => true,
            _ => false,
        }
    }
}

/// What a check found a module to be.
enum Found {
    /// A member, checked from source
    Member(Arc<Member>),
    /// A member, checked against a typelib of it
    Typelib(Arc<Member>, Arc<Written>),
    Bundled(Arc<Library>),
    Missing,
}

/// A bundled module's typelib, and the modules a check of it needs.
pub(crate) struct Library {
    typelib: typeck::Typelib<'static>,
    depends: Vec<String>,
}

/// What a check of a workspace is to do.
pub(crate) enum Round {
    /// Its table changed under the check, which must wait for it to settle
    Changed,
    /// Check each of these, which may be none
    Checks(Vec<Check>),
}

/// Members to check together, and the typelibs of the rest of what they
/// import.
pub(crate) struct Check {
    members: Vec<Arc<Member>>,
    /// The members checked against their typelibs
    written: Vec<Interface>,
    typelibs: Vec<Arc<Library>>,
    /// The members the check is of, as indexes into `members`, and the
    /// closures it is checked against
    targets: Vec<(usize, Deps)>,
}

impl Check {
    /// The members checked from source.
    pub(crate) fn members(&self) -> &[Arc<Member>] {
        &self.members
    }

    /// The members checked against their typelibs.  A member whose unit
    /// fails to compile may be both: checked from source, and its last good
    /// typelib checked against.
    pub(crate) fn written(&self) -> &[Interface] {
        &self.written
    }

    /// The bundled typelibs checked against, sorted by module.
    pub(crate) fn typelibs(&self) -> impl Iterator<Item = &typeck::Typelib<'static>> {
        self.typelibs.iter().map(|library| &library.typelib)
    }
}

/// A member a check takes as a typelib.
pub(crate) struct Interface {
    member: Arc<Member>,
    written: Arc<Written>,
    /// Whether the typelib was written of the member's own unit, rather than
    /// being its last good
    own: bool,
}

impl Interface {
    pub(crate) fn member(&self) -> &Arc<Member> {
        &self.member
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.written.bytes
    }

    /// Whether the typelib was written of the member's own unit, which then
    /// locates its declarations, rather than being its last good.
    pub(crate) fn own(&self) -> bool {
        self.own
    }
}

impl Member {
    /// The client's version of the document, if the client has it open.
    pub(crate) fn version(&self) -> Option<i32> {
        match self.source {
            Source::Open { version, .. } => Some(version),
            Source::Loaded { .. } => None,
        }
    }

    /// The client's text of the document, if the client has it open.
    pub(crate) fn text(&self) -> Option<&str> {
        match &self.source {
            Source::Open { text, .. } => Some(text),
            Source::Loaded { .. } => None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The module it is on the module path, or `None` for a script.
    pub(crate) fn module(&self) -> Option<&str> {
        self.module.as_deref()
    }

    pub(crate) fn uri(&self) -> Option<&Uri> {
        match &self.source {
            Source::Open { uri, .. } => Some(uri),
            Source::Loaded { .. } => None,
        }
    }

    /// Its latest check, if it has one, valid or not.
    pub(crate) fn checked(&self) -> Option<Checked> {
        Some(self.results().as_ref()?.checked.clone())
    }

    /// The closure of its latest check, or else of an earlier revision's.
    fn deps(&self) -> Option<Deps> {
        let results = self.results();
        results
            .as_ref()
            .map_or(self.earlier.clone(), |results| Some(results.deps.clone()))
    }

    /// The typelib its latest check wrote, if it has one, else its last good.
    fn last_good(&self) -> Option<Arc<Written>> {
        let results = self.results();
        let typelib = results.as_ref().and_then(|results| results.typelib.clone());
        typelib.or_else(|| self.last_good.clone())
    }

    fn results(&self) -> MutexGuard<'_, Option<Results>> {
        self.results.lock().expect("results lock poisoned")
    }

    /// Its unit, which a member is once a check has gathered it.
    fn compiled(&self) -> &Unit<'static> {
        self.unit.get().expect("a gathered member is compiled")
    }

    /// The modules a check of its unit needs, which a member has once a check
    /// has gathered it. A unit that failed to compile needs none.
    fn depends(&self) -> &[String] {
        self.checkable().map_or(&[], Vec::as_slice)
    }

    /// Whether the checker can't take its unit, which failed to compile.
    fn fails(&self) -> bool {
        self.checkable().is_none()
    }

    fn checkable(&self) -> Option<&Vec<String>> {
        self.depends
            .get_or_init(|| {
                let mut check = typeck::Check::new();
                check.pipes(PIPES[0], PIPES[1]);
                let id = check.unit(self.compiled()).ok()?;
                Some(owned(check.depends(id)))
            })
            .as_ref()
    }

    /// Of the members of a module, the one it looks up to: the client's
    /// buffer before a file, and of each the least path.
    fn rank(&self) -> (bool, &Path) {
        (self.uri().is_none(), &self.path)
    }
}

/// The member each module looks up to among `members`.
fn index<'a>(
    members: impl IntoIterator<Item = &'a Arc<Member>>,
) -> HashMap<&'a str, &'a Arc<Member>> {
    let mut index = HashMap::<&str, &Arc<Member>>::new();
    for member in members {
        if let Some(name) = &member.module {
            let entry = index.entry(name).or_insert(member);
            if member.rank() < entry.rank() {
                *entry = member;
            }
        }
    }
    index
}

impl Workspace {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The directories of the module path that exist now, in order.
    pub(crate) fn module_dirs(&self) -> Vec<PathBuf> {
        self.modules().map_or_else(Vec::new, |modules| {
            modules
                .roots()
                .iter()
                .filter(|root| root.is_dir())
                .cloned()
                .collect()
        })
    }

    /// The module the document at `path` is on the module path, if it is.
    pub(crate) fn module_of(&self, path: &Path) -> Option<String> {
        self.modules()?.module_of(path)
    }

    /// The prelude the document at `path` is compiled with: that of the
    /// nearest configuration below the root that sets one, else the root's,
    /// which a buffer with no file has too. An unreadable configuration sets
    /// an empty prelude.
    pub(crate) fn prelude(&self, path: &Path) -> Arc<[Import]> {
        let Some(dir) = path.parent().filter(|dir| dir.starts_with(&self.root)) else {
            return self.prelude.clone();
        };
        if dir == self.root {
            return self.prelude.clone();
        }
        if let Some(prelude) = self.preludes().get(dir) {
            return prelude.clone();
        }
        let config = dir.join(CONFIG_FILE_NAME);
        let prelude = match config.is_file() {
            true => match settings::read(&config, None) {
                Ok(settings) => settings.prelude.map(Arc::from),
                Err(err) => {
                    log::error!("failed to load configuration {}: {err}", config.display());
                    Some(Arc::default())
                }
            },
            false => None,
        };
        // A configuration may leave the prelude to the enclosing one
        let prelude = prelude.unwrap_or_else(|| self.prelude(dir));
        self.preludes().insert(dir.to_owned(), prelude.clone());
        prelude
    }

    fn preludes(&self) -> MutexGuard<'_, HashMap<PathBuf, Arc<[Import]>>> {
        self.preludes.lock().expect("prelude lock poisoned")
    }

    fn modules(&self) -> Option<&ModulePath> {
        self.modules.as_deref()
    }

    /// The current revision of the file at `path`.
    #[cfg(test)]
    fn member(&self, path: &Path) -> Option<Arc<Member>> {
        self.members().get(&Key::File(path.to_owned())).cloned()
    }

    /// Whether `member` is still its document's current revision.
    pub(crate) fn current(&self, member: &Arc<Member>) -> bool {
        current(&self.members(), member)
    }

    /// Makes the client's `text` of the document `key` its current revision,
    /// as `version`, unless the client has a newer version open there
    /// already.  A buffer with no file is a script.
    ///
    /// The same text keeps the unit of the revision it replaces, and its
    /// check if the client had it open too; other text is compiled once
    /// something needs it.
    pub(crate) fn revise(
        &self,
        uri: Uri,
        key: Key,
        version: i32,
        text: String,
    ) -> Option<Arc<Member>> {
        let path = key.path();
        let module = match &key {
            Key::File(path) => self.module_of(path),
            Key::Untitled(_) => None,
        };
        let mut members = self.members();
        let old = members.get(&key);
        if let Some(Source::Open { version: newer, .. }) = old.map(|old| &old.source)
            && *newer > version
        {
            return None;
        }
        let unit = old
            .and_then(|old| old.unit.get())
            .filter(|unit| unit.source() == text)
            .cloned();
        // A unit doesn't depend on its version, but a loaded member's check
        // isn't located in a document the client has open
        let results = old
            .filter(|old| unit.is_some() && old.uri().is_some())
            .and_then(|old| old.results().clone());
        let member = Arc::new(Member {
            key,
            module,
            path,
            source: Source::Open { uri, version, text },
            unit: unit.map(OnceLock::from).unwrap_or_default(),
            depends: OnceLock::new(),
            results: Mutex::new(results),
            earlier: old.and_then(|old| old.deps()),
            last_good: old.and_then(|old| old.last_good()),
        });
        members.insert(member.key.clone(), member.clone());
        Some(member)
    }

    /// A member's unit, compiled the first time it is needed.
    pub(crate) fn unit(&self, member: &Member) -> Arc<Unit<'static>> {
        member
            .unit
            .get_or_init(|| {
                let Source::Open { text, .. } = &member.source else {
                    unreachable!("a loaded member is compiled when loaded");
                };
                let module = member.module.as_deref();
                Arc::new(compile_document(
                    &member.path,
                    text.clone(),
                    &self.prelude(&member.path),
                    module,
                ))
            })
            .clone()
    }

    /// Forgets the document `key` the client closed.
    pub(crate) fn close(&self, key: &Key) {
        self.members().remove(key);
    }

    /// What a check of the workspace is to check now.
    ///
    /// Loaded members no open document imports are dropped first.  Then the
    /// files of the modules the rest loaded or looked for are looked at; if
    /// any changed, the table does, and the check must wait.
    /// Otherwise the members whose checks aren't valid are checked together
    /// with the closure of their imports, loading modules it lacks.  A
    /// client's buffer of a module that another buffer of the module stands
    /// in for is checked on its own.
    pub(crate) fn prepare(&self) -> Round {
        let snapshot: Vec<_> = self.members().values().cloned().collect();
        let snapshot = self.collect(snapshot);
        if self.disk(&snapshot) {
            return Round::Changed;
        }
        let index = index(&snapshot);
        let mut roots = Vec::new();
        let mut shadowed = Vec::new();
        for member in &snapshot {
            if self.valid(member, &index) {
                continue;
            }
            match member.module.as_deref() {
                Some(name) if !Arc::ptr_eq(index[name], member) => {
                    shadowed.push(vec![member.clone()]);
                }
                _ => roots.push(member.clone()),
            }
        }
        let whole = (!roots.is_empty()).then(|| self.gather(roots, true, &index));
        let checks: Vec<_> = whole
            .into_iter()
            .chain((shadowed.into_iter()).map(|roots| self.gather(roots, false, &index)))
            .collect();
        // A member replaced meanwhile makes the check moot, and its
        // replacement will be checked once the table settles
        let members = self.members();
        let replaced = checks
            .iter()
            .flat_map(|check| {
                let written = check.written.iter().map(|interface| &interface.member);
                check.members.iter().chain(written)
            })
            .any(|member| !current(&members, member));
        match replaced {
            true => Round::Changed,
            false => Round::Checks(checks),
        }
    }

    /// Drops the loaded members of `snapshot` that no member the client has
    /// open imports, directly or transitively, as its latest check found,
    /// returning the rest.
    ///
    /// A revision not checked yet holds what its earlier revision's check
    /// found.  The modules a check found are looked up again by name, so a
    /// member that replaced one, or a file that appeared for one, is reached
    /// too.  A module only an edit imports may be dropped and loaded again;
    /// that costs only the read.
    fn collect(&self, snapshot: Vec<Arc<Member>>) -> Vec<Arc<Member>> {
        let index = index(&snapshot);
        let mut reached = HashSet::new();
        for member in snapshot.iter().filter(|member| member.uri().is_some()) {
            reached.insert(Arc::as_ptr(member));
            for (name, _) in member.deps().iter().flat_map(|deps| deps.iter()) {
                reached.extend(index.get(name.as_str()).map(|member| Arc::as_ptr(member)));
            }
        }
        let (kept, dropped): (Vec<_>, Vec<_>) = snapshot
            .iter()
            .partition(|member| reached.contains(&Arc::as_ptr(member)));
        if !dropped.is_empty() {
            let mut members = self.members();
            for member in dropped {
                if current(&members, member) {
                    members.remove(&member.key);
                }
            }
        }
        kept.into_iter().cloned().collect()
    }

    /// Brings the loaded members up to their files, and loads the modules
    /// checks looked for and found missing or bundled whose files have
    /// appeared, returning whether any changed.
    ///
    /// A module the client has open is its buffer, not its file, so a change
    /// to its file changes nothing.
    fn disk(&self, snapshot: &[Arc<Member>]) -> bool {
        let Some(modules) = self.modules() else {
            return false;
        };
        let index = index(snapshot);
        let mut names = BTreeSet::new();
        for member in snapshot {
            match &member.source {
                Source::Loaded { .. } => names.extend(member.module.clone()),
                Source::Open { .. } => {}
            }
            let results = member.results();
            let deps = results.iter().flat_map(|results| results.deps.iter());
            for (name, resolution) in deps {
                if !matches!(resolution, Resolution::Member(_)) {
                    names.insert(name.clone());
                }
            }
        }
        let mut changed = false;
        for name in names {
            let old = index.get(name.as_str()).copied();
            let old = match old.map(|old| &old.source) {
                Some(Source::Open { .. }) => continue,
                Some(Source::Loaded { stamp }) => old.map(|old| (old, stamp)),
                None => None,
            };
            match (modules.locate(&name), old) {
                (None, None) => continue,
                (Some((path, stamp)), Some((old, old_stamp)))
                    if path == old.path && stamp == *old_stamp =>
                {
                    continue;
                }
                _ => {}
            }
            changed = true;
            let last_good = old.and_then(|(old, _)| old.last_good());
            let new = self.load(modules, &name, last_good);
            let mut members = self.members();
            if let Some((old, _)) = old
                && current(&members, old)
            {
                members.remove(&old.key);
            }
            if let Some(new) = new {
                admit(&mut members, new, true);
            }
        }
        changed
    }

    /// Whether `member` has a check that is still valid: whether each module
    /// in its closure still looks up the same in `index`.
    fn valid(&self, member: &Member, index: &HashMap<&str, &Arc<Member>>) -> bool {
        let results = member.results();
        let Some(results) = &*results else {
            return false;
        };
        results
            .deps
            .iter()
            .all(|(name, resolution)| self.resolve(index, name).same(resolution))
    }

    /// What a module name looks up to in `index`: a member, else its bundled
    /// typelib if the module path allows.
    fn resolve(&self, index: &HashMap<&str, &Arc<Member>>, name: &str) -> Resolution {
        match index.get(name) {
            Some(member) => Resolution::Member(Arc::downgrade(member)),
            None if self.typelib(name).is_some() => Resolution::Bundled,
            None => Resolution::Missing,
        }
    }

    /// A module's bundled typelib, if the module path allows.
    fn typelib(&self, name: &str) -> Option<Arc<Library>> {
        self.modules()
            .is_none_or(|modules| modules.bundled)
            .then(|| self.bundled.get(name))
            .flatten()
    }

    /// `roots`, compiled, and the closure of their imports: the members they
    /// look up to, and those loaded from disk for modules no member is.
    /// Each root stands for its own module.
    ///
    /// A member with a valid check that wrote its typelib is taken as that
    /// typelib, unless it is a root.  A member whose unit fails to compile is
    /// checked against its last good typelib, if it has one, and a root of
    /// them is checked from source too, for its own check.
    ///
    /// The check is of the roots and the members with no check yet when
    /// `whole`, else of the first root alone, whose own module another
    /// member stands for.
    fn gather(
        &self,
        roots: Vec<Arc<Member>>,
        whole: bool,
        index: &HashMap<&str, &Arc<Member>>,
    ) -> Check {
        let mut found = HashMap::new();
        let mut queue: Vec<String> = Vec::new();
        let mut written = Vec::new();
        for root in &roots {
            self.unit(root);
            let last_good = root.last_good.clone().filter(|_| root.fails());
            let Some(name) = root.module.clone() else {
                queue.extend(root.depends().iter().cloned());
                continue;
            };
            let next = match last_good {
                Some(last_good) => {
                    queue.extend(last_good.depends.iter().cloned());
                    written.push(Interface {
                        member: root.clone(),
                        written: last_good.clone(),
                        own: false,
                    });
                    Found::Typelib(root.clone(), last_good)
                }
                None => {
                    queue.extend(root.depends().iter().cloned());
                    Found::Member(root.clone())
                }
            };
            found.insert(name, next);
        }
        let count = roots.len();
        let mut members = roots;
        let mut typelibs = Vec::new();
        while let Some(name) = queue.pop() {
            if found.contains_key(&name) {
                continue;
            }
            let full = members.len() >= MAX_MODULES;
            let next = match self.find(&name, full) {
                Found::Member(member) => match self.interface(&member, index) {
                    Some((typelib, own)) => {
                        written.push(Interface {
                            member: member.clone(),
                            written: typelib.clone(),
                            own,
                        });
                        Found::Typelib(member, typelib)
                    }
                    None => Found::Member(member),
                },
                next => next,
            };
            match &next {
                Found::Member(member) => {
                    queue.extend(member.depends().iter().cloned());
                    members.push(member.clone());
                }
                Found::Typelib(_, typelib) => queue.extend(typelib.depends.iter().cloned()),
                Found::Bundled(library) => {
                    queue.extend(library.depends.iter().cloned());
                    typelibs.push(library.clone());
                }
                Found::Missing => {}
            }
            found.insert(name, next);
        }
        typelibs.sort_by(|a, b| a.typelib.module().cmp(b.typelib.module()));
        let targets = (0..members.len())
            .filter(|&target| match whole {
                true => target < count || members[target].results().is_none(),
                false => target == 0,
            })
            .map(|target| (target, closure(&found, &members[target])))
            .collect();
        Check {
            members,
            written,
            typelibs,
            targets,
        }
    }

    /// The typelib a check takes `member` as, and whether it was written of
    /// the member's own unit: the one its valid check wrote, else its last
    /// good if its unit fails to compile.
    fn interface(
        &self,
        member: &Member,
        index: &HashMap<&str, &Arc<Member>>,
    ) -> Option<(Arc<Written>, bool)> {
        self.unit(member);
        if member.fails() {
            return Some((member.last_good.clone()?, false));
        }
        let typelib = member.results().as_ref()?.typelib.clone()?;
        self.valid(member, index).then_some((typelib, true))
    }

    /// What a module is now: the member it looks up to, else its source on
    /// the module path, loaded, else its bundled typelib.  A check `full`
    /// of sources reads no more.
    fn find(&self, name: &str, full: bool) -> Found {
        let member = self
            .members()
            .values()
            .filter(|member| member.module.as_deref() == Some(name))
            .min_by(|a, b| a.rank().cmp(&b.rank()))
            .cloned();
        let member = member.or_else(|| {
            let loaded = self.load(self.modules()?, name, None).filter(|_| !full)?;
            Some(admit(&mut self.members(), loaded, false))
        });
        match member {
            Some(_) if full => {
                log::error!("a check needs more than {MAX_MODULES} module sources");
                Found::Missing
            }
            Some(member) => Found::Member(member),
            None => match self.typelib(name) {
                Some(typelib) => Found::Bundled(typelib),
                None => Found::Missing,
            },
        }
    }

    /// A module's source on the module path, read and compiled, with the last
    /// good typelib of the member it replaces.
    fn load(
        &self,
        modules: &ModulePath,
        name: &str,
        last_good: Option<Arc<Written>>,
    ) -> Option<Arc<Member>> {
        let (path, stamp, text) = modules.open(name)?;
        let unit = compile_document(&path, text, &self.prelude(&path), Some(name));
        Some(Arc::new(Member {
            key: Key::File(path.clone()),
            path,
            module: Some(name.to_owned()),
            source: Source::Loaded { stamp },
            unit: OnceLock::from(Arc::new(unit)),
            depends: OnceLock::new(),
            results: Mutex::default(),
            earlier: None,
            last_good,
        }))
    }

    /// Stores what `check` made of the members it is of, as each one's
    /// diagnostics in `checked` and the typelib written of it in `typelibs`,
    /// returning those it stored for: those still current.
    pub(crate) fn settle(
        &self,
        check: Check,
        mut checked: Vec<Checked>,
        mut typelibs: Typelibs,
    ) -> Vec<Arc<Member>> {
        let targets: Vec<_> = {
            let members = self.members();
            check
                .targets
                .into_iter()
                .filter(|(target, _)| current(&members, &check.members[*target]))
                .collect()
        };
        targets
            .into_iter()
            .map(|(target, deps)| {
                let member = &check.members[target];
                let checked = std::mem::take(&mut checked[target]);
                let typelib = typelibs[target].take().map(|bytes| {
                    let depends = member.depends().to_vec();
                    Arc::new(Written { bytes, depends })
                });
                *member.results() = Some(Results {
                    checked,
                    deps,
                    typelib,
                });
                member.clone()
            })
            .collect()
    }

    fn members(&self) -> MutexGuard<'_, HashMap<Key, Arc<Member>>> {
        self.members.lock().expect("workspace lock poisoned")
    }
}

/// Whether `member` is its document's current revision in `members`.
fn current(members: &HashMap<Key, Arc<Member>>, member: &Arc<Member>) -> bool {
    members
        .get(&member.key)
        .is_some_and(|current| Arc::ptr_eq(current, member))
}

/// Makes a `loaded` member current, unless the client has a buffer of its
/// module or its document open, returning what its module looks up to now.
/// Unless `replace`, a member already loaded at its path is kept.
fn admit(
    members: &mut HashMap<Key, Arc<Member>>,
    loaded: Arc<Member>,
    replace: bool,
) -> Arc<Member> {
    let open = members
        .values()
        .filter(|member| member.uri().is_some() && member.module == loaded.module)
        .min_by(|a, b| a.rank().cmp(&b.rank()));
    if let Some(open) = open {
        return open.clone();
    }
    match members.get(&loaded.key) {
        Some(there) if there.uri().is_some() || !replace => there.clone(),
        _ => {
            members.insert(loaded.key.clone(), loaded.clone());
            loaded
        }
    }
}

/// What each module `member` imports, directly or transitively, was `found`
/// to be.  Its own module is itself.
fn closure(found: &HashMap<String, Found>, member: &Member) -> Deps {
    let mut seen: HashSet<&str> = member.module.as_deref().into_iter().collect();
    let mut queue: Vec<&str> = member.depends().iter().map(String::as_str).collect();
    let mut deps = Vec::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let resolution = match &found[name] {
            Found::Member(member) => {
                queue.extend(member.depends().iter().map(String::as_str));
                Resolution::Member(Arc::downgrade(member))
            }
            Found::Typelib(member, typelib) => {
                queue.extend(typelib.depends.iter().map(String::as_str));
                Resolution::Member(Arc::downgrade(member))
            }
            Found::Bundled(library) => {
                queue.extend(library.depends.iter().map(String::as_str));
                Resolution::Bundled
            }
            Found::Missing => Resolution::Missing,
        };
        deps.push((name.to_owned(), resolution));
    }
    deps.into()
}

fn owned(names: Vec<&str>) -> Vec<String> {
    names.into_iter().map(str::to_owned).collect()
}

/// The bundled typelibs, decoded as checks first need them, for every
/// workspace.
#[derive(Default)]
struct Bundled(Mutex<HashMap<String, Option<Arc<Library>>>>);

impl Bundled {
    /// A bundled module's typelib, decoded the first time a check needs it.
    fn get(&self, name: &str) -> Option<Arc<Library>> {
        let mut typelibs = self.0.lock().expect("bundled lock poisoned");
        typelibs
            .entry(name.to_owned())
            .or_insert_with(|| {
                let bytes = typelib_index::lookup(name)?;
                let typelib = typeck::Typelib::read(bytes)
                    .inspect_err(|err| log::error!("bundled typelib {name} is unreadable: {err}"))
                    .ok()?;
                let mut check = typeck::Check::new();
                check.pipes(PIPES[0], PIPES[1]);
                let depends = match check.typelib(&typelib) {
                    Ok(id) => owned(check.depends(id)),
                    Err(_) => Vec::new(),
                };
                Some(Arc::new(Library { typelib, depends }))
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{backend::check_units, module_path::tests::Directory};

    fn open(workspace: &Workspace, path: &Path, version: i32, text: &str) -> Option<Arc<Member>> {
        let uri = Uri::from_file_path(path).unwrap();
        let key = Key::File(path.to_owned());
        workspace.revise(uri, key, version, text.to_owned())
    }

    /// The checks a check of the workspace makes, or `None` if its table
    /// changed under it.
    fn prepare(workspace: &Workspace) -> Option<Vec<Check>> {
        match workspace.prepare() {
            Round::Changed => None,
            Round::Checks(checks) => Some(checks),
        }
    }

    /// The paths of a check's members, sorted, and of the members it is of.
    fn paths(check: &Check) -> (Vec<&Path>, Vec<&Path>) {
        let mut members: Vec<_> = check
            .members
            .iter()
            .map(|member| member.path.as_path())
            .collect();
        members.sort();
        let mut targets: Vec<_> = check
            .targets
            .iter()
            .map(|(target, _)| check.members[*target].path.as_path())
            .collect();
        targets.sort();
        (members, targets)
    }

    /// Settles what checking the workspace now makes of it.
    fn check(workspace: &Workspace) -> Vec<Arc<Member>> {
        let checks = prepare(workspace).expect("the table is settled");
        checks
            .into_iter()
            .flat_map(|check| {
                let checked = vec![Checked::default(); check.members.len()];
                let typelibs = vec![None; check.members.len()];
                workspace.settle(check, checked, typelibs)
            })
            .collect()
    }

    /// Settles what checking the workspace now makes of it, as if each check
    /// wrote the typelib of every module whose unit compiled.
    fn check_writing(workspace: &Workspace) -> Vec<Arc<Member>> {
        let checks = prepare(workspace).expect("the table is settled");
        checks
            .into_iter()
            .flat_map(|check| {
                let checked = vec![Checked::default(); check.members.len()];
                let typelibs = (check.members.iter())
                    .map(|member| {
                        let writes = member.module.is_some() && !member.fails();
                        writes.then(|| Box::from(&b""[..]))
                    })
                    .collect();
                workspace.settle(check, checked, typelibs)
            })
            .collect()
    }

    /// The sorted paths of the members a check takes as typelibs, and whether
    /// each is its own.
    fn written(check: &Check) -> Vec<(&Path, bool)> {
        let mut written: Vec<_> = (check.written.iter())
            .map(|interface| (interface.member.path.as_path(), interface.own))
            .collect();
        written.sort();
        written
    }

    fn typelib(member: &Member) -> bool {
        member
            .results()
            .as_ref()
            .is_some_and(|results| results.typelib.is_some())
    }

    #[test]
    fn valid_members_are_checked_as_their_typelibs() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "import b\n");
        let b = tree.write("lib/b.dol", "");
        let main = tree.0.join("main.dol");
        let other = tree.0.join("other.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();
        check_writing(&workspace);

        // A later check takes the modules the first checked as their typelibs
        open(&workspace, &other, 1, "import a\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        let [check] = &checks[..] else {
            panic!("expected one check");
        };
        assert_eq!(paths(check), (vec![other.as_path()], vec![other.as_path()]));
        assert_eq!(written(check), [(a.as_path(), true), (&b, true)]);
        check_writing(&workspace);

        // An edit to `b` drops its typelib and those of its importers, which
        // are checked from source again
        open(&workspace, &b, 1, "# b\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        let [check] = &checks[..] else {
            panic!("expected one check");
        };
        let all = vec![a.as_path(), &b, &main, &other];
        assert_eq!(paths(check), (all.clone(), all));
        assert_eq!(written(check), []);
    }

    #[test]
    fn a_module_that_fails_to_compile_stands_as_its_last_good() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "import b\n");
        let b = tree.write("lib/b.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();
        check_writing(&workspace);

        // Broken, `a` is checked from source for its own check, and its
        // importers against its last good typelib, with what that imports
        let broken = open(&workspace, &a, 1, "pub def\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        assert!(broken.fails());
        let [check] = &checks[..] else {
            panic!("expected one check");
        };
        let roots = vec![a.as_path(), &main];
        assert_eq!(paths(check), (roots.clone(), roots));
        assert_eq!(written(check), [(a.as_path(), false), (&b, true)]);
        check_writing(&workspace);
        assert!(!typelib(&broken));

        // It stays the last good through further broken revisions
        open(&workspace, &a, 2, "pub def\n# still\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        assert_eq!(written(&checks[0]), [(a.as_path(), false), (&b, true)]);
        check_writing(&workspace);

        // Fixed, its source replaces it
        let fixed = open(&workspace, &a, 3, "import b\n# fixed\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        assert_eq!(written(&checks[0]), [(b.as_path(), true)]);
        check_writing(&workspace);
        assert!(typelib(&fixed));
    }

    #[test]
    fn checks_write_typelibs_only_of_modules_whose_imports_resolve() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "pub def f x@Int\n  x\n");
        let c = tree.write("lib/c.dol", "import nowhere\npub def g x\n  x\n");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\nimport c\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        for check in checks {
            let (checked, typelibs) = check_units(&workspace, &check, &HashMap::new());
            workspace.settle(check, checked, typelibs);
        }
        assert!(typelib(&workspace.member(&a).unwrap()));
        assert!(!typelib(&workspace.member(&c).unwrap()));
        assert!(!typelib(&workspace.member(&main).unwrap()));

        // A broken module has none
        let broken = open(&workspace, &a, 1, "pub def\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        for check in checks {
            let (checked, typelibs) = check_units(&workspace, &check, &HashMap::new());
            workspace.settle(check, checked, typelibs);
        }
        assert!(broken.checked().is_some());
        assert!(!typelib(&broken));
    }

    fn library(tree: &Directory) {
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
    }

    #[test]
    fn documents_share_the_workspace_of_their_repository() {
        let tree = Directory::repo();
        // A configuration below the root doesn't split the workspace
        tree.write(&format!("nested/{CONFIG_FILE_NAME}"), "[prelude]\n");
        // A submodule's `.git` file makes it its own root
        tree.write("sub/module/.git", "gitdir: ../../.git/modules/module\n");
        let mut workspaces = Workspaces::default();
        let main = workspaces.open(&tree.0.join("main.dol"));
        assert_eq!(main.root, tree.0);
        let other = workspaces.open(&tree.0.join("sub/other.dol"));
        assert!(Arc::ptr_eq(&main, &other));
        let nested = workspaces.open(&tree.0.join("nested/a.dol"));
        assert!(Arc::ptr_eq(&main, &nested));
        let module = workspaces.open(&tree.0.join("sub/module/lib/a.dol"));
        assert_eq!(module.root, tree.0.join("sub/module"));
        // So does a project's `doze.toml`, with the convention's module path
        tree.write(&format!("sub/app/{PROJECT_FILE_NAME}"), "");
        tree.write("sub/app/lib/x/mod.dol", "");
        let app = workspaces.open(&tree.0.join("sub/app/lib/x/mod.dol"));
        assert_eq!(app.root, tree.0.join("sub/app"));
        assert_eq!(
            app.module_of(&tree.0.join("sub/app/lib/x/mod.dol"))
                .as_deref(),
            Some("x")
        );

        // A change to a configuration retires the workspaces containing it
        workspaces.retire(&tree.0.join("nested"));
        assert!(!workspaces.current(&main));
        assert!(workspaces.current(&module));
        workspaces.retire(&tree.0.join("sub/module/x"));
        assert!(!workspaces.current(&module));
        let reopened = workspaces.open(&tree.0.join("main.dol"));
        assert!(!Arc::ptr_eq(&main, &reopened));

        // Once nothing holds a workspace, it's gone
        drop((main, other, nested, reopened));
        assert!(workspaces.live().all(|live| live.root != tree.0));

        // With no repository, a document's own directory.  That needs a
        // scratch directory no root encloses, which a temporary directory
        // under a checkout, or a stray `C:\.git` under Wine, isn't.
        let loose = Directory::new();
        let enclosed = loose.0.ancestors().any(marks_root);
        if !enclosed {
            let script = workspaces.open(&loose.0.join("a/b.dol"));
            assert_eq!(script.root, loose.0.join("a"));
        }
    }

    #[test]
    fn the_convention_puts_lib_on_the_module_path() {
        let tree = Directory::repo();
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        assert_eq!(&*workspace.prelude(&main), settings::default_prelude());
        open(&workspace, &main, 1, "import a\n").unwrap();
        check(&workspace);
        assert_eq!(prepare(&workspace).unwrap().len(), 0);

        // `lib/` appears after the workspace opened
        let a = tree.write("lib/a.dol", "");
        assert_eq!(workspace.module_of(&a).as_deref(), Some("a"));
        assert_eq!(workspace.module_of(&main), None);
        assert!(prepare(&workspace).is_none());
        assert_eq!(check_paths(&workspace), [a.as_path(), &main]);
    }

    #[test]
    fn subtree_configurations_override_the_prelude() {
        let tree = Directory::repo();
        // The root's configuration leaves the prelude to the convention
        tree.write(CONFIG_FILE_NAME, "[modules]\npath = [\"src\"]\n");
        tree.write(
            &format!("tests/{CONFIG_FILE_NAME}"),
            "[prelude]\nfoo = true\n",
        );
        tree.write(&format!("tests/deep/{CONFIG_FILE_NAME}"), "");
        tree.write(
            &format!("tests/bad/{CONFIG_FILE_NAME}"),
            "[prelude]\nfoo = true\n[modules]\npath = [\"lib\"]\n",
        );
        let src = tree.write("src/m.dol", "");
        let workspace = Workspaces::default().open(&tree.0.join("main.dol"));
        let foo = [Import::Module("foo".into())];
        assert_eq!(&*workspace.prelude(&tree.0.join("tests/x.dol")), foo);
        assert_eq!(&*workspace.prelude(&tree.0.join("tests/deep/y.dol")), foo);
        assert_eq!(
            &*workspace.prelude(&tree.0.join("main.dol")),
            settings::default_prelude()
        );
        assert_eq!(
            &*workspace.prelude(&tree.0.join("other/z.dol")),
            settings::default_prelude()
        );

        // A `modules` table below the root is rejected, leaving its subtree
        // an empty prelude and the root's module path as it was
        assert_eq!(&*workspace.prelude(&tree.0.join("tests/bad/w.dol")), []);
        assert_eq!(workspace.module_of(&src).as_deref(), Some("m"));
        let lib = tree.write("tests/bad/lib/n.dol", "");
        assert_eq!(workspace.module_of(&lib), None);

        // The root's own prelude applies where nothing overrides it
        tree.write(CONFIG_FILE_NAME, "[prelude]\n");
        let workspace = Workspaces::default().open(&tree.0.join("main.dol"));
        assert_eq!(&*workspace.prelude(&tree.0.join("other/z.dol")), []);
        assert_eq!(&*workspace.prelude(&tree.0.join("tests/x.dol")), foo);
    }

    #[test]
    fn checks_gather_the_closure_of_unchecked_members() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "import b\n");
        let b = tree.write("lib/b.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();

        // Through `a`, `b` is loaded, and the check is of all three
        let checks = prepare(&workspace).unwrap();
        let [check] = &checks[..] else {
            panic!("expected one check");
        };
        let mut all = vec![a.as_path(), &b, &main];
        all.sort();
        assert_eq!(paths(check), (all.clone(), all));
        assert!(workspace.member(&b).is_some_and(|b| b.uri().is_none()));

        // Once settled, nothing is left to check
        assert_eq!(check_paths(&workspace), [a.as_path(), &b, &main]);
        assert_eq!(prepare(&workspace).unwrap().len(), 0);
    }

    /// The sorted paths of the members checking the workspace settles.
    fn check_paths(workspace: &Workspace) -> Vec<PathBuf> {
        let mut paths: Vec<_> = check(workspace)
            .into_iter()
            .map(|member| member.path.clone())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn checks_stay_valid_while_their_imports_look_up_the_same() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "import b\n");
        let b = tree.write("lib/b.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();
        check(&workspace);

        // A buffer of `b` stands in for its file, invalidating its importers
        open(&workspace, &b, 1, "# b\n").unwrap();
        assert_eq!(check_paths(&workspace), [a.as_path(), &b, &main]);
        assert!(workspace.member(&b).unwrap().uri().is_some());

        // An edit to `main` leaves `a` and `b` alone
        open(&workspace, &main, 2, "import a\n# edited\n").unwrap();
        assert_eq!(check_paths(&workspace), [main.as_path()]);

        // Closed, the buffer's file stands in again
        workspace.close(&Key::File(b.clone()));
        assert_eq!(check_paths(&workspace), [a.as_path(), &b, &main]);
        assert!(workspace.member(&b).is_some_and(|b| b.uri().is_none()));

        // A change to the file replaces its member, and the check waits
        let loaded = workspace.member(&b).unwrap();
        fs::write(&b, "# changed on disk\n").unwrap();
        assert!(prepare(&workspace).is_none());
        assert!(!workspace.current(&loaded));
        assert_eq!(check_paths(&workspace), [a.as_path(), &b, &main]);

        // Unreachable from a buffer's check, loaded members are dropped
        // before the next
        open(&workspace, &main, 3, "").unwrap();
        assert_eq!(check_paths(&workspace), [main.as_path()]);
        assert!(workspace.member(&a).is_some());
        assert_eq!(prepare(&workspace).unwrap().len(), 0);
        assert!(workspace.member(&a).is_none());
        assert!(workspace.member(&b).is_none());
    }

    #[test]
    fn loaded_members_stay_while_an_open_document_imports_them() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "");
        let main = tree.0.join("main.dol");
        let other = tree.0.join("other.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();
        check(&workspace);
        let loaded = workspace.member(&a).unwrap();

        // Through its last check, `other` holds `a` once `main` lets it go,
        // whatever check `main` is due
        open(&workspace, &other, 1, "import a\n").unwrap();
        assert_eq!(check_paths(&workspace), [other.as_path()]);
        open(&workspace, &main, 2, "").unwrap();
        assert_eq!(check_paths(&workspace), [main.as_path()]);
        assert!(workspace.current(&loaded));

        // Unchecked, an edit holds what the revision it replaced was checked
        // against; once checked, what its own check found
        open(&workspace, &other, 2, "").unwrap();
        assert_eq!(check_paths(&workspace), [other.as_path()]);
        assert!(workspace.current(&loaded));
        assert_eq!(prepare(&workspace).unwrap().len(), 0);
        assert!(workspace.member(&a).is_none());
    }

    #[test]
    fn missing_modules_are_looked_for_again() {
        let tree = Directory::repo();
        library(&tree);
        tree.write("lib/a.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import c\n").unwrap();
        check(&workspace);
        assert_eq!(prepare(&workspace).unwrap().len(), 0);
        let c = tree.write("lib/c.dol", "");
        assert!(prepare(&workspace).is_none());
        assert_eq!(check_paths(&workspace), [c.as_path(), &main]);
    }

    #[test]
    fn checks_of_replaced_members_are_passed_over() {
        let tree = Directory::repo();
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        let first = open(&workspace, &main, 1, "let a = 1\n").unwrap();
        let checks = prepare(&workspace).unwrap();
        let second = open(&workspace, &main, 2, "let b = 1\n").unwrap();
        for check in checks {
            let settled = workspace.settle(check, vec![Checked::default()], vec![None]);
            assert!(settled.is_empty());
        }
        assert!(first.checked().is_none());
        assert!(second.checked().is_none());
        assert_eq!(check(&workspace).len(), 1);
    }

    #[test]
    fn a_buffer_of_a_module_another_stands_in_for_is_checked_alone() {
        let tree = Directory::repo();
        tree.write(
            CONFIG_FILE_NAME,
            "[prelude]\n[modules]\npath = [\"one\", \"two\"]\n",
        );
        let one = tree.write("one/a.dol", "");
        let two = tree.write("two/a.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        open(&workspace, &main, 1, "import a\n").unwrap();
        open(&workspace, &one, 1, "").unwrap();
        open(&workspace, &two, 1, "").unwrap();
        let checks = prepare(&workspace).unwrap();
        let checks: Vec<_> = checks.iter().map(paths).collect();
        assert_eq!(
            checks,
            [
                (vec![main.as_path(), &one], vec![main.as_path(), &one]),
                (vec![two.as_path()], vec![two.as_path()]),
            ]
        );
    }

    /// Opens an `untitled:` buffer named `name`.
    fn untitled(workspace: &Workspace, name: &str, text: &str) -> Arc<Member> {
        let uri: Uri = format!("untitled:{name}").parse().unwrap();
        let key = Key::Untitled(uri.clone());
        workspace.revise(uri, key, 1, text.to_owned()).unwrap()
    }

    #[test]
    fn buffers_without_files_are_scripts_in_the_first_folder() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "");
        let mut workspaces = Workspaces::default();
        workspaces.set_folder(Some(tree.0.clone()));
        let key = Key::Untitled("untitled:a.dol".parse().unwrap());
        let workspace = workspaces.of(&key);
        assert_eq!(workspace.root, tree.0);

        // Named like a module, it is still a script, importing the module
        let buffer = untitled(&workspace, "a.dol", "import a\n");
        assert_eq!(buffer.module, None);
        assert_eq!(buffer.path, Path::new("a.dol"));
        assert_eq!(check_paths(&workspace), [a.as_path(), Path::new("a.dol")]);

        // Nothing else imports it
        let main = tree.0.join("main.dol");
        open(&workspace, &main, 1, "import a\n").unwrap();
        check(&workspace);
        let checked = workspace.member(&a).unwrap();
        assert!(checked.uri().is_none());

        // Closed, it is forgotten
        workspace.close(&key);
        assert!(workspace.members().get(&key).is_none());
    }

    #[test]
    fn buffers_without_a_folder_are_detached() {
        let mut workspaces = Workspaces::default();
        let key = Key::Untitled("untitled:Untitled-1".parse().unwrap());
        let workspace = workspaces.of(&key);
        assert!(workspaces.current(&workspace));
        assert!(Arc::ptr_eq(&workspace, &workspaces.detached()));
        assert!(workspaces.live().any(|live| Arc::ptr_eq(&live, &workspace)));
        assert_eq!(
            &*workspace.prelude(Path::new("Untitled-1")),
            settings::default_prelude()
        );

        // Only the bundled modules resolve
        untitled(&workspace, "Untitled-1", "import json\nimport nowhere\n");
        let checks = prepare(&workspace).unwrap();
        let [check] = &checks[..] else {
            panic!("expected one check");
        };
        assert_eq!(check.members.len(), 1);
        let modules: Vec<_> = check.typelibs().map(|typelib| typelib.module()).collect();
        // Ordinary builds bundle none
        let bundled = typelib_index::lookup("json").is_some();
        assert_eq!(modules.contains(&"json"), bundled);
        assert!(!modules.contains(&"nowhere"));

        // No configuration retires it
        workspaces.retire(Path::new("/"));
        assert!(workspaces.current(&workspace));
    }

    #[test]
    fn revisions_replace_by_version() {
        let tree = Directory::repo();
        library(&tree);
        let a = tree.write("lib/a.dol", "");
        let main = tree.0.join("main.dol");
        let workspace = Workspaces::default().open(&main);
        let first = open(&workspace, &main, 2, "import a\n").unwrap();

        // An older version is passed over, a newer one replaces it
        assert!(open(&workspace, &main, 1, "# older\n").is_none());
        let second = open(&workspace, &main, 3, "# newer\n").unwrap();
        assert!(!workspace.current(&first));
        assert!(workspace.current(&second));

        // The same text keeps its unit and check
        check(&workspace);
        let third = open(&workspace, &main, 4, "# newer\n").unwrap();
        assert!(Arc::ptr_eq(
            &workspace.unit(&second),
            &workspace.unit(&third)
        ));
        assert!(third.checked().is_some());

        // Opening a loaded module's text keeps its unit, but not its check
        open(&workspace, &main, 5, "import a\n").unwrap();
        check(&workspace);
        let loaded = workspace.member(&a).unwrap();
        assert!(loaded.checked().is_some());
        let opened = open(&workspace, &a, 1, "").unwrap();
        assert!(Arc::ptr_eq(
            &workspace.unit(&loaded),
            &workspace.unit(&opened)
        ));
        assert!(opened.checked().is_none());
    }
}
