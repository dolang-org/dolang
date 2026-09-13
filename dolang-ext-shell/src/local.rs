use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    mem,
    ops::Deref,
    rc::Rc,
};

use dolang::runtime::{State, Strand, strand};
use dolang_vfs::{
    Vfs,
    process::Signal,
    security::SecurityInfo,
    target::{OperatingSystem, OperatingSystemFamily, TargetInfo},
};

use crate::{global::Global, shell_args::ArgsData};
use dolang_vfs::path as vfs_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminationPolicy {
    pub(crate) signal: Signal,
    pub(crate) grace: std::time::Duration,
    pub(crate) force: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            signal: Signal::Term,
            grace: std::time::Duration::from_secs(5),
            force: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Env {
    parent: Option<Rc<Env>>,
    vars: HashMap<String, Option<String>>,
    baseline: bool,
    family: OperatingSystemFamily,
}

impl Env {
    pub(crate) fn new(
        parent: Option<Rc<Env>>,
        baseline: bool,
        values: impl IntoIterator<Item = (String, String)>,
        operating_system: OperatingSystem,
    ) -> Self {
        let family = operating_system.family();
        Self {
            parent,
            baseline,
            vars: values
                .into_iter()
                .map(|(k, v)| (Self::normalize_key(family, k), Some(v)))
                .collect(),
            family,
        }
    }

    pub(crate) fn derived(parent: Rc<Env>, values: HashMap<String, Option<String>>) -> Self {
        let family = parent.family;
        Self {
            parent: Some(parent),
            baseline: false,
            vars: values
                .into_iter()
                .map(|(key, value)| (Self::normalize_key(family, key), value))
                .collect(),
            family,
        }
    }

    fn normalize_key(family: OperatingSystemFamily, key: impl Into<String>) -> String {
        let key = key.into();
        match family {
            OperatingSystemFamily::Unix => key,
            OperatingSystemFamily::Windows => key.to_ascii_uppercase(),
        }
    }

    pub(crate) fn get<'a>(&'a self, key: &str) -> Option<Cow<'a, str>> {
        let key = Self::normalize_key(self.family, key);
        match self.vars.get(&key) {
            Some(None) => None,
            Some(Some(value)) => Some(Cow::Borrowed(value.as_str())),
            None => {
                if let Some(parent) = &self.parent {
                    parent.get(&key)
                } else {
                    None
                }
            }
        }
    }

    pub(crate) fn insert(&mut self, key: String, value: Option<String>) {
        self.vars
            .insert(Self::normalize_key(self.family, key), value);
    }

    fn baseline(&self) -> &HashMap<String, Option<String>> {
        if self.baseline {
            &self.vars
        } else {
            self.parent
                .as_ref()
                .expect("derived env missing parent")
                .baseline()
        }
    }

    fn flatten_delta_into(&self, out: &mut HashMap<String, Option<String>>) {
        if self.baseline {
            return;
        }
        if let Some(parent) = &self.parent {
            parent.flatten_delta_into(out);
        }
        out.extend(self.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    pub(crate) fn flatten_delta(&self) -> HashMap<String, Option<String>> {
        let mut out = HashMap::new();
        self.flatten_delta_into(&mut out);
        out
    }

    pub(crate) fn effective_map(&self) -> HashMap<String, String> {
        let baseline = self.baseline();
        let delta = self.flatten_delta();
        let mut out = HashMap::new();

        for (key, value) in baseline {
            match delta.get(key) {
                Some(Some(value)) => {
                    out.insert(key.clone(), value.clone());
                }
                Some(None) => {}
                None => {
                    if let Some(value) = value {
                        out.insert(key.clone(), value.clone());
                    }
                }
            }
        }

        for (key, value) in delta {
            if let Some(value) = value
                && !baseline.contains_key(&key)
            {
                out.insert(key, value);
            }
        }

        out
    }

    pub(crate) fn visit(&self, f: &mut impl FnMut(&str, Option<&str>)) {
        if !self.baseline {
            if let Some(parent) = &self.parent {
                parent.visit(f);
            }
            for (k, v) in self.vars.iter() {
                f(k, v.as_deref())
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum ProgramOverride {
    Path(vfs_path::PathBuf),
    Module(Box<str>),
}

#[derive(Clone, Default)]
pub(crate) struct InvocationOverride {
    pub(crate) args: Option<ArgsData>,
    pub(crate) program: Option<ProgramOverride>,
}

pub(crate) struct Local {
    cwd: RefCell<vfs_path::PathBuf>,
    env: RefCell<Rc<Env>>,
    vfs: RefCell<Vfs>,
    background: Cell<bool>,
    termination_policy: RefCell<TerminationPolicy>,
    invocation: RefCell<InvocationOverride>,
}

impl<'v> strand::Local<'v> for Local {
    fn init() -> Self {
        let vfs = Vfs::direct().expect("failed to initialize direct VFS");
        Self {
            cwd: RefCell::new(vfs.cwd().to_path_buf()),
            env: RefCell::new(Rc::new(Env::derived(
                Rc::new(Env::new(None, true, vfs.env(), vfs.target().os())),
                Default::default(),
            ))),
            vfs: RefCell::new(vfs),
            background: Cell::new(false),
            termination_policy: RefCell::new(TerminationPolicy::default()),
            invocation: RefCell::new(InvocationOverride::default()),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>, kind: strand::InheritKind) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            vfs: self.vfs.clone(),
            background: Cell::new(self.background.get() || kind == strand::InheritKind::Background),
            termination_policy: self.termination_policy.clone(),
            invocation: self.invocation.clone(),
        }
    }
}

impl Local {
    pub(crate) fn env(&self) -> Rc<Env> {
        self.env.borrow().clone()
    }

    pub(crate) fn cwd(&self) -> impl Deref<Target = vfs_path::PathBuf> {
        self.cwd.borrow()
    }

    pub(crate) fn replace_cwd(&self, cwd: vfs_path::PathBuf) -> vfs_path::PathBuf {
        mem::replace(&mut *self.cwd.borrow_mut(), cwd)
    }

    pub(crate) fn replace_env(&self, env: Rc<Env>) -> Rc<Env> {
        mem::replace(&mut *self.env.borrow_mut(), env)
    }

    pub(crate) fn replace_vfs(&self, vfs: Vfs) -> Vfs {
        mem::replace(&mut *self.vfs.borrow_mut(), vfs)
    }

    pub(crate) fn vfs(&self) -> Vfs {
        self.vfs.borrow().clone()
    }

    pub(crate) fn vfs_exe(&self) -> Option<vfs_path::PathBuf> {
        let vfs = self.vfs.borrow();
        (!vfs.is_direct()).then(|| vfs.current_exe().to_path_buf())
    }

    pub(crate) fn target(&self) -> TargetInfo {
        self.vfs.borrow().target().clone()
    }

    pub(crate) fn security(&self) -> SecurityInfo {
        self.vfs.borrow().security().clone()
    }

    pub(crate) async fn with_vfs<'v, 's, R>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        vfs: Vfs,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
    ) -> R {
        let cwd = vfs.cwd().to_path_buf();
        let env = Rc::new(Env::new(None, true, vfs.env(), vfs.target().os()));
        let local = global.local.get(strand);
        let orig_vfs = local.replace_vfs(vfs);
        let orig_cwd = local.replace_cwd(cwd);
        let orig_env = local.replace_env(Rc::new(Env::derived(env, HashMap::new())));
        let result = f(strand).await;
        let local = global.local.get(strand);
        local.replace_vfs(orig_vfs);
        local.replace_cwd(orig_cwd);
        local.replace_env(orig_env);
        result
    }

    pub(crate) fn background(&self) -> bool {
        self.background.get()
    }

    pub(crate) fn termination_policy(&self) -> TerminationPolicy {
        self.termination_policy.borrow().clone()
    }

    pub(crate) fn replace_termination_policy(
        &self,
        policy: TerminationPolicy,
    ) -> TerminationPolicy {
        mem::replace(&mut self.termination_policy.borrow_mut(), policy)
    }

    pub(crate) fn invocation(&self) -> InvocationOverride {
        self.invocation.borrow().clone()
    }

    pub(crate) fn replace_invocation(&self, invocation: InvocationOverride) -> InvocationOverride {
        mem::replace(&mut *self.invocation.borrow_mut(), invocation)
    }
}

#[cfg(test)]
mod tests {
    use super::Env;
    use dolang_vfs::target::OperatingSystem;
    use std::{collections::HashMap, rc::Rc};

    #[test]
    fn windows_environment_keys_are_case_insensitive_across_layers() {
        let root = Rc::new(Env::new(
            None,
            true,
            [("Path".to_owned(), "base".to_owned())],
            OperatingSystem::Windows,
        ));
        let mut env = Env::derived(
            root,
            HashMap::from([("pAtH".to_owned(), Some("override".to_owned()))]),
        );

        assert_eq!(env.get("PATH").as_deref(), Some("override"));
        env.insert("path".to_owned(), None);
        assert_eq!(env.get("PaTh"), None);
        assert_eq!(
            env.flatten_delta(),
            HashMap::from([("PATH".to_owned(), None)])
        );
        assert!(env.effective_map().is_empty());
    }

    #[test]
    fn unix_environment_keys_remain_case_sensitive() {
        let root = Rc::new(Env::new(
            None,
            true,
            [("Path".to_owned(), "mixed".to_owned())],
            OperatingSystem::Linux,
        ));
        let env = Env::derived(
            root,
            HashMap::from([("PATH".to_owned(), Some("upper".to_owned()))]),
        );

        assert_eq!(env.get("Path").as_deref(), Some("mixed"));
        assert_eq!(env.get("PATH").as_deref(), Some("upper"));
    }
}
