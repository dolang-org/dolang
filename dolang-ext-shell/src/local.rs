use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    env, mem,
    ops::Deref,
    rc::Rc,
};

use dolang::runtime::{Strand, strand};
use dolang_shell_vfs::AnyVfs;
use dolang_shell_vfs::{Utf8TypedPathBuf, typed_path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelMode {
    Line,
    Chunk,
}

#[derive(Clone)]
pub(crate) struct Env {
    parent: Option<Rc<Env>>,
    vars: HashMap<String, Option<String>>,
    baseline: bool,
}

impl Env {
    pub(crate) fn root() -> Self {
        Self {
            parent: None,
            baseline: true,
            #[cfg(not(windows))]
            vars: env::vars().map(|(k, v)| (k, Some(v))).collect(),
            #[cfg(windows)]
            vars: env::vars()
                .map(|(k, v)| (k.to_ascii_uppercase(), Some(v)))
                .collect(),
        }
    }

    pub(crate) fn new(
        parent: Option<Rc<Env>>,
        baseline: bool,
        values: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            parent,
            baseline,
            #[cfg(not(windows))]
            vars: values.into_iter().map(|(k, v)| (k, Some(v))).collect(),
            #[cfg(windows)]
            vars: values
                .into_iter()
                .map(|(k, v)| (k.to_ascii_uppercase(), Some(v)))
                .collect(),
        }
    }

    pub(crate) fn derived(parent: Rc<Env>, values: HashMap<String, Option<String>>) -> Self {
        Self {
            parent: Some(parent),
            baseline: false,
            vars: values,
        }
    }

    pub(crate) fn get<'a>(&'a self, key: &str) -> Option<Cow<'a, str>> {
        match self.vars.get(key) {
            Some(None) => None,
            Some(Some(value)) => Some(Cow::Borrowed(value.as_str())),
            None => {
                if let Some(parent) = &self.parent {
                    parent.get(key)
                } else {
                    None
                }
            }
        }
    }

    pub(crate) fn insert(&mut self, key: String, value: Option<String>) {
        self.vars.insert(key, value);
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

pub(crate) struct Local {
    cwd: RefCell<Utf8TypedPathBuf>,
    env: RefCell<Rc<Env>>,
    vfs: RefCell<AnyVfs>,
    channel_mode: Cell<ChannelMode>,
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
            channel_mode: Cell::new(ChannelMode::Line),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            vfs: self.vfs.clone(),
            channel_mode: Cell::new(self.channel_mode.get()),
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

    pub(crate) fn channel_mode(&self) -> ChannelMode {
        self.channel_mode.get()
    }

    pub(crate) fn set_channel_mode(&self, v: ChannelMode) {
        self.channel_mode.set(v);
    }
}
