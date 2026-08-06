use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    env, mem,
    ops::Deref,
    rc::Rc,
};

use dolang::runtime::{Strand, strand};
use dolang_vfs::{
    AnyVfs, OperatingSystem, OperatingSystemFamily, SecurityInfo, Signal, TargetInfo,
};
use dolang_vfs::{Utf8TypedPathBuf, typed_path};

use crate::{io_mode::IoMode, shell_args::ArgsData};

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
    pub(crate) fn root() -> Self {
        Self::new(None, true, env::vars(), OperatingSystem::current())
    }

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
    Path(Utf8TypedPathBuf),
    Module(Box<str>),
}

#[derive(Clone, Default)]
pub(crate) struct InvocationOverride {
    pub(crate) args: Option<ArgsData>,
    pub(crate) program: Option<ProgramOverride>,
}

pub(crate) struct Local {
    cwd: RefCell<Utf8TypedPathBuf>,
    env: RefCell<Rc<Env>>,
    vfs: RefCell<AnyVfs>,
    vfs_exe: RefCell<Option<Utf8TypedPathBuf>>,
    target: RefCell<TargetInfo>,
    security: RefCell<Option<SecurityInfo>>,
    io_mode: Cell<IoMode>,
    background: Cell<bool>,
    /// Set while dispatching a write into the ambient console.
    ///
    /// A console written in Do may itself call `echo`; without this guard that
    /// would route straight back into the same console and recurse until the
    /// call-depth limit. While set, console writes bypass the capture and go to
    /// the host.
    capturing: Cell<bool>,
    /// The `can_style` the ambient console reported when it was installed.
    ///
    /// Snapshotted rather than read live because `can_style` is defined to be
    /// fixed for the life of an installed console — which is what makes a
    /// capture's styling deterministic — and because the styling query is a
    /// sync, infallible one reachable from a public Rust API.
    ///
    /// Only meaningful while a capture is installed; the host answers from
    /// `Terminal::ansi` instead.
    capture_can_style: Cell<bool>,
    /// The `is_tty` the ambient console reported when it was installed.
    ///
    /// Snapshotted for the same reason as [`Self::capture_can_style`]: the
    /// question is defined to be fixed for the life of an installed console.
    ///
    /// Only meaningful while a capture is installed; the host answers from
    /// `Terminal::stderr_is_terminal` instead.
    capture_is_tty: Cell<bool>,
    termination_policy: RefCell<TerminationPolicy>,
    invocation: RefCell<InvocationOverride>,
    /// One-shot hint consumed by `pipe_channel`'s pipe factory: the buffer
    /// size the *next* native OS pipe it creates on this strand should be
    /// given, if any. Set immediately before triggering pipe creation and
    /// cleared immediately after, rather than plumbed through `stream`'s
    /// public signature — the pipe factory override is already an
    /// internal-only mechanism, so this stays consistent with that.
    pending_pipe_buffer_size: Cell<Option<usize>>,
}

impl<'v> strand::Local<'v> for Local {
    fn init() -> Self {
        Self {
            cwd: RefCell::new(typed_path(env::current_dir().unwrap()).unwrap()),
            env: RefCell::new(Rc::new(Env::derived(
                Rc::new(Env::root()),
                Default::default(),
            ))),
            vfs: RefCell::new(AnyVfs::default()),
            vfs_exe: RefCell::new(None),
            target: RefCell::new(TargetInfo::current()),
            security: RefCell::new(None),
            io_mode: Cell::new(IoMode::Line),
            background: Cell::new(false),
            capturing: Cell::new(false),
            capture_can_style: Cell::new(false),
            capture_is_tty: Cell::new(false),
            termination_policy: RefCell::new(TerminationPolicy::default()),
            invocation: RefCell::new(InvocationOverride::default()),
            pending_pipe_buffer_size: Cell::new(None),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>, kind: strand::InheritKind) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            vfs: self.vfs.clone(),
            vfs_exe: self.vfs_exe.clone(),
            target: self.target.clone(),
            security: self.security.clone(),
            io_mode: Cell::new(self.io_mode.get()),
            background: Cell::new(self.background.get() || kind == strand::InheritKind::Background),
            // Inherited so that a strand spawned from inside a console's own
            // write stays guarded rather than routing back into it.
            capturing: Cell::new(self.capturing.get()),
            // Inherited alongside the capture root itself, so a strand spawned
            // inside a capture answers the styling question the same way.
            capture_can_style: Cell::new(self.capture_can_style.get()),
            capture_is_tty: Cell::new(self.capture_is_tty.get()),
            termination_policy: self.termination_policy.clone(),
            invocation: self.invocation.clone(),
            // Transient, one-shot; never meant to cross a strand boundary.
            pending_pipe_buffer_size: Cell::new(None),
        }
    }
}

impl Local {
    pub(crate) fn env(&self) -> Rc<Env> {
        self.env.borrow().clone()
    }

    pub(crate) fn cwd(&self) -> impl Deref<Target = Utf8TypedPathBuf> {
        self.cwd.borrow()
    }

    pub(crate) fn replace_cwd(&self, cwd: Utf8TypedPathBuf) -> Utf8TypedPathBuf {
        mem::replace(&mut *self.cwd.borrow_mut(), cwd)
    }

    pub(crate) fn replace_env(&self, env: Rc<Env>) -> Rc<Env> {
        mem::replace(&mut *self.env.borrow_mut(), env)
    }

    pub(crate) fn replace_vfs(&self, vfs: AnyVfs) -> AnyVfs {
        mem::replace(&mut *self.vfs.borrow_mut(), vfs)
    }

    pub(crate) fn vfs(&self) -> AnyVfs {
        self.vfs.borrow().clone()
    }

    pub(crate) fn vfs_exe(&self) -> Option<Utf8TypedPathBuf> {
        self.vfs_exe.borrow().clone()
    }

    pub(crate) fn replace_vfs_exe(
        &self,
        exe: Option<Utf8TypedPathBuf>,
    ) -> Option<Utf8TypedPathBuf> {
        mem::replace(&mut *self.vfs_exe.borrow_mut(), exe)
    }

    pub(crate) fn target(&self) -> TargetInfo {
        self.target.borrow().clone()
    }

    pub(crate) fn replace_target(&self, target: TargetInfo) -> TargetInfo {
        mem::replace(&mut *self.target.borrow_mut(), target)
    }

    pub(crate) fn security(&self) -> Option<SecurityInfo> {
        self.security.borrow().clone()
    }

    pub(crate) fn replace_security(&self, security: Option<SecurityInfo>) -> Option<SecurityInfo> {
        mem::replace(&mut *self.security.borrow_mut(), security)
    }

    pub(crate) fn io_mode(&self) -> IoMode {
        self.io_mode.get()
    }

    pub(crate) fn set_io_mode(&self, v: IoMode) {
        self.io_mode.set(v);
    }

    pub(crate) fn set_pending_pipe_buffer_size(&self, size: Option<usize>) {
        self.pending_pipe_buffer_size.set(size);
    }

    pub(crate) fn pending_pipe_buffer_size(&self) -> Option<usize> {
        self.pending_pipe_buffer_size.get()
    }

    pub(crate) fn background(&self) -> bool {
        self.background.get()
    }

    pub(crate) fn capturing(&self) -> bool {
        self.capturing.get()
    }

    pub(crate) fn set_capturing(&self, v: bool) -> bool {
        self.capturing.replace(v)
    }

    pub(crate) fn capture_can_style(&self) -> bool {
        self.capture_can_style.get()
    }

    pub(crate) fn set_capture_can_style(&self, v: bool) -> bool {
        self.capture_can_style.replace(v)
    }

    pub(crate) fn capture_is_tty(&self) -> bool {
        self.capture_is_tty.get()
    }

    pub(crate) fn set_capture_is_tty(&self, v: bool) -> bool {
        self.capture_is_tty.replace(v)
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
    use dolang_vfs::OperatingSystem;
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
