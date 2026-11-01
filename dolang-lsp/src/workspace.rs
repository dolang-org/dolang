//! Workspaces, and the documents open in each.
//!
//! A workspace is a root, the nearest directory above a document with a
//! `.git` or else the document's own directory, with its settings: those of
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
    settings::{self, CONFIG_FILE_NAME, Import},
    typelib_index,
};

/// The most module sources one check reads
const MAX_MODULES: usize = 512;

/// The live workspaces, by root.
#[derive(Default)]
pub(crate) struct Workspaces {
    table: HashMap<PathBuf, Weak<Workspace>>,
    bundled: Arc<Bundled>,
}

impl Workspaces {
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

    /// Whether `workspace` is still the one its root opens.
    pub(crate) fn current(&self, workspace: &Arc<Workspace>) -> bool {
        self.table
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
        self.table.values().filter_map(Weak::upgrade)
    }
}

/// The workspace root of the documents in `dir`: the nearest of it and its
/// ancestors with a `.git`, whether a repository's directory or a submodule's
/// or worktree's file, else `dir`.
fn root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
        .unwrap_or(dir)
        .to_owned()
}

/// One root, and its members.
pub(crate) struct Workspace {
    root: PathBuf,
    /// The prelude at the root
    prelude: Arc<[Import]>,
    /// `None` when the root's configuration is unreadable
    modules: Option<Arc<ModulePath>>,
    /// The prelude of each directory below the root looked at so far
    preludes: Mutex<HashMap<PathBuf, Arc<[Import]>>>,
    bundled: Arc<Bundled>,
    /// The current revision of each member
    members: Mutex<HashMap<PathBuf, Arc<Member>>>,
}

/// One revision of a document in a workspace.
pub(crate) struct Member {
    path: PathBuf,
    /// The module it is on the module path, or `None` for a script
    module: Option<String>,
    source: Source,
    /// Compiled when first needed, which for an open document is on the
    /// worker rather than in the handler that made it
    unit: OnceLock<Arc<Unit<'static>>>,
    results: Mutex<Option<Results>>,
    /// The closure of the latest check of an earlier revision, which keeps
    /// its modules loaded until this one is checked
    earlier: Option<Deps>,
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
    Member(Arc<Member>),
    Bundled(Arc<typeck::Typelib<'static>>),
    Missing,
}

/// What a check of a workspace is to do.
pub(crate) enum Round {
    /// Its table changed under the check, which must wait for it to settle
    Changed,
    /// Check each of these, which may be none
    Checks(Vec<Check>),
}

/// Members to check together, and the bundled typelibs of the rest of what
/// they import.
pub(crate) struct Check {
    members: Vec<Arc<Member>>,
    typelibs: Vec<Arc<typeck::Typelib<'static>>>,
    /// The members the check is of, as indexes into `members`, and the
    /// closures it is checked against
    targets: Vec<(usize, Deps)>,
}

impl Check {
    /// The members checked from source.
    pub(crate) fn members(&self) -> &[Arc<Member>] {
        &self.members
    }

    /// The bundled typelibs checked against, sorted by module.
    pub(crate) fn typelibs(&self) -> &[Arc<typeck::Typelib<'static>>] {
        &self.typelibs
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

    fn results(&self) -> MutexGuard<'_, Option<Results>> {
        self.results.lock().expect("results lock poisoned")
    }

    /// Its unit, which a member is once a check has gathered it.
    fn compiled(&self) -> &Unit<'static> {
        self.unit.get().expect("a gathered member is compiled")
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
    /// nearest configuration below the root that sets one, else the root's.
    /// An unreadable configuration sets an empty prelude.
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

    /// The current revision of the document at `path`.
    #[cfg(test)]
    fn member(&self, path: &Path) -> Option<Arc<Member>> {
        self.members().get(path).cloned()
    }

    /// Whether `member` is still its document's current revision.
    pub(crate) fn current(&self, member: &Arc<Member>) -> bool {
        current(&self.members(), member)
    }

    /// Makes the client's `text` of the document at `path` its current
    /// revision, as `version`, unless the client has a newer version open
    /// there already.
    ///
    /// The same text keeps the unit of the revision it replaces, and its
    /// check if the client had it open too; other text is compiled once
    /// something needs it.
    pub(crate) fn revise(
        &self,
        uri: Uri,
        path: PathBuf,
        version: i32,
        text: String,
    ) -> Option<Arc<Member>> {
        let module = self.module_of(&path);
        let mut members = self.members();
        let old = members.get(&path);
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
            module,
            path,
            source: Source::Open { uri, version, text },
            unit: unit.map(OnceLock::from).unwrap_or_default(),
            results: Mutex::new(results),
            earlier: old.and_then(|old| old.deps()),
        });
        members.insert(member.path.clone(), member.clone());
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

    /// Forgets the document the client closed at `path`.
    pub(crate) fn close(&self, path: &Path) {
        self.members().remove(path);
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
        let whole = (!roots.is_empty()).then(|| self.gather(roots, true));
        let checks: Vec<_> = whole
            .into_iter()
            .chain(shadowed.into_iter().map(|roots| self.gather(roots, false)))
            .collect();
        // A member replaced meanwhile makes the check moot, and its
        // replacement will be checked once the table settles
        let members = self.members();
        let replaced = checks
            .iter()
            .flat_map(|check| &check.members)
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
                    members.remove(&member.path);
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
            let new = self.load(modules, &name);
            let mut members = self.members();
            if let Some((old, _)) = old
                && current(&members, old)
            {
                members.remove(&old.path);
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
    fn typelib(&self, name: &str) -> Option<Arc<typeck::Typelib<'static>>> {
        self.modules()
            .is_none_or(|modules| modules.bundled)
            .then(|| self.bundled.get(name))
            .flatten()
    }

    /// `roots`, compiled, and the closure of their imports: the members they
    /// look up to, and those loaded from disk for modules no member is.
    /// Each root stands for its own module.
    ///
    /// The check is of the roots and the members with no check yet when
    /// `whole`, else of the first root alone, whose own module another
    /// member stands for.
    fn gather(&self, roots: Vec<Arc<Member>>, whole: bool) -> Check {
        let mut found: HashMap<String, Found> = roots
            .iter()
            .filter_map(|root| Some((root.module.clone()?, Found::Member(root.clone()))))
            .collect();
        let mut queue: Vec<String> = Vec::new();
        for root in &roots {
            queue.extend(imports(&self.unit(root)).map(str::to_owned));
        }
        let count = roots.len();
        let mut members = roots;
        let mut typelibs = Vec::new();
        while let Some(name) = queue.pop() {
            if found.contains_key(&name) {
                continue;
            }
            let full = members.len() >= MAX_MODULES;
            let next = self.find(&name, full);
            match &next {
                Found::Member(member) => {
                    queue.extend(imports(&self.unit(member)).map(str::to_owned));
                    members.push(member.clone());
                }
                Found::Bundled(typelib) => {
                    queue.extend(typelib.imports().into_iter().map(str::to_owned));
                    typelibs.push(typelib.clone());
                }
                Found::Missing => {}
            }
            found.insert(name, next);
        }
        typelibs.sort_by(|a, b| a.module().cmp(b.module()));
        let targets = (0..members.len())
            .filter(|&target| match whole {
                true => target < count || members[target].results().is_none(),
                false => target == 0,
            })
            .map(|target| (target, closure(&found, &members[target])))
            .collect();
        Check {
            members,
            typelibs,
            targets,
        }
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
            let loaded = self.load(self.modules()?, name).filter(|_| !full)?;
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

    /// A module's source on the module path, read and compiled.
    fn load(&self, modules: &ModulePath, name: &str) -> Option<Arc<Member>> {
        let (path, stamp, text) = modules.open(name)?;
        let unit = compile_document(&path, text, &self.prelude(&path), Some(name));
        Some(Arc::new(Member {
            path,
            module: Some(name.to_owned()),
            source: Source::Loaded { stamp },
            unit: OnceLock::from(Arc::new(unit)),
            results: Mutex::default(),
            earlier: None,
        }))
    }

    /// Stores what `check` made of the members it is of, as each one's
    /// diagnostics in `checked`, returning those it stored for: those still
    /// current.
    pub(crate) fn settle(&self, check: Check, mut checked: Vec<Checked>) -> Vec<Arc<Member>> {
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
                *member.results() = Some(Results { checked, deps });
                member.clone()
            })
            .collect()
    }

    fn members(&self) -> MutexGuard<'_, HashMap<PathBuf, Arc<Member>>> {
        self.members.lock().expect("workspace lock poisoned")
    }
}

/// Whether `member` is its document's current revision in `members`.
fn current(members: &HashMap<PathBuf, Arc<Member>>, member: &Arc<Member>) -> bool {
    members
        .get(&member.path)
        .is_some_and(|current| Arc::ptr_eq(current, member))
}

/// Makes a `loaded` member current, unless the client has a buffer of its
/// module or its document open, returning what its module looks up to now.
/// Unless `replace`, a member already loaded at its path is kept.
fn admit(
    members: &mut HashMap<PathBuf, Arc<Member>>,
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
    match members.get(&loaded.path) {
        Some(there) if there.uri().is_some() || !replace => there.clone(),
        _ => {
            members.insert(loaded.path.clone(), loaded.clone());
            loaded
        }
    }
}

/// What each module `member` imports, directly or transitively, was `found`
/// to be.  Its own module is itself.
fn closure(found: &HashMap<String, Found>, member: &Member) -> Deps {
    let mut seen: HashSet<&str> = member.module.as_deref().into_iter().collect();
    let mut queue: Vec<&str> = imports(member.compiled()).collect();
    let mut deps = Vec::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let resolution = match &found[name] {
            Found::Member(member) => {
                queue.extend(imports(member.compiled()));
                Resolution::Member(Arc::downgrade(member))
            }
            Found::Bundled(typelib) => {
                queue.extend(typelib.imports());
                Resolution::Bundled
            }
            Found::Missing => Resolution::Missing,
        };
        deps.push((name.to_owned(), resolution));
    }
    deps.into()
}

/// What a check of `unit` needs first: its imports, plus `proc` for the pipe
/// types `strand` stands for.
fn imports<'a>(unit: &'a Unit<'_>) -> impl Iterator<Item = &'a str> {
    unit.imports().into_iter().chain(["proc"])
}

/// The bundled typelibs, decoded as checks first need them, for every
/// workspace.
#[derive(Default)]
struct Bundled(Mutex<HashMap<String, Option<Arc<typeck::Typelib<'static>>>>>);

impl Bundled {
    /// A bundled module's typelib, decoded the first time a check needs it.
    fn get(&self, name: &str) -> Option<Arc<typeck::Typelib<'static>>> {
        let mut typelibs = self.0.lock().expect("bundled lock poisoned");
        typelibs
            .entry(name.to_owned())
            .or_insert_with(|| {
                let bytes = typelib_index::lookup(name)?;
                typeck::Typelib::read(bytes)
                    .inspect_err(|err| log::error!("bundled typelib {name} is unreadable: {err}"))
                    .ok()
                    .map(Arc::new)
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::module_path::tests::Directory;

    fn open(workspace: &Workspace, path: &Path, version: i32, text: &str) -> Option<Arc<Member>> {
        let uri = Uri::from_file_path(path).unwrap();
        workspace.revise(uri, path.to_owned(), version, text.to_owned())
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
                workspace.settle(check, checked)
            })
            .collect()
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
        // scratch directory no repository encloses, which a temporary
        // directory under a checkout, or a stray `C:\.git` under Wine, isn't.
        let loose = Directory::new();
        let enclosed = loose
            .0
            .ancestors()
            .any(|dir| fs::symlink_metadata(dir.join(".git")).is_ok());
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
        workspace.close(&b);
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
            assert!(workspace.settle(check, vec![Checked::default()]).is_empty());
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
