use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    env, mem,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    rc::Rc,
};

use dolang::runtime::{Strand, strand};

use crate::container::Context;

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
            #[cfg(not(target_os = "windows"))]
            vars: env::vars().map(|(k, v)| (k, Some(v))).collect(),
            #[cfg(target_os = "windows")]
            vars: env::vars()
                .map(|(k, v)| (k.to_ascii_uppercase(), Some(v)))
                .collect(),
        }
    }

    #[cfg(unix)]
    pub(crate) fn new(
        parent: Option<Rc<Env>>,
        baseline: bool,
        values: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            parent,
            baseline,
            vars: values.into_iter().map(|(k, v)| (k, Some(v))).collect(),
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
    cwd: RefCell<PathBuf>,
    env: RefCell<Rc<Env>>,
    agent: RefCell<Option<Context>>,
    binary_mode: Cell<bool>,
}

impl<'v> strand::Local<'v> for Local {
    fn init() -> Self {
        Self {
            cwd: RefCell::new(env::current_dir().unwrap()),
            env: RefCell::new(Rc::new(Env::root())),
            agent: RefCell::new(None),
            binary_mode: Cell::new(false),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            agent: self.agent.clone(),
            binary_mode: Cell::new(self.binary_mode.get()),
        }
    }
}

impl Local {
    pub(crate) fn env(&self) -> Rc<Env> {
        self.env.borrow().clone()
    }

    pub(crate) fn cwd(&self) -> impl Deref<Target = impl AsRef<Path>> {
        self.cwd.borrow()
    }

    pub(crate) fn replace_cwd(&self, cwd: impl Into<PathBuf>) -> PathBuf {
        mem::replace(&mut *self.cwd.borrow_mut(), cwd.into())
    }

    pub(crate) fn replace_env(&self, env: Rc<Env>) -> Rc<Env> {
        mem::replace(&mut *self.env.borrow_mut(), env)
    }

    pub(crate) fn container(&self) -> impl Deref<Target = Option<Context>> {
        self.agent.borrow()
    }

    pub(crate) fn container_mut(&self) -> impl DerefMut<Target = Option<Context>> {
        self.agent.borrow_mut()
    }

    #[cfg(unix)]
    pub(crate) fn binary_mode(&self) -> bool {
        self.binary_mode.get()
    }

    pub(crate) fn set_binary_mode(&self, v: bool) {
        self.binary_mode.set(v);
    }
}
