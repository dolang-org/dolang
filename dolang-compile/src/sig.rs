use crate::constant;

use super::sym;

use dolang_bytecode::Variadic;
use dolang_util::intern;

pub(crate) struct PackTag;

pub(crate) type PackId = intern::Id<PackTag>;

#[cfg(feature = "debug")]
use super::Compiler;

#[cfg(feature = "debug")]
use std::fmt::{self, Write};

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub(crate) enum Arg {
    Value,
    Pack,
    Key(sym::Id),
}

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub(crate) struct Pack(Vec<Arg>);

impl Pack {
    pub(crate) fn new(iter: impl Iterator<Item = Arg>) -> Self {
        Self(iter.collect())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arg> {
        self.0.iter()
    }

    #[expect(dead_code)]
    pub(crate) fn into_iter(self) -> impl Iterator<Item = Arg> {
        self.0.into_iter()
    }
}

#[cfg(feature = "debug")]
impl Pack {
    pub fn dump(&self, compiler: &Compiler, w: &mut impl Write) -> fmt::Result {
        let mut pos = 0;
        let mut need_space = false;
        for p in self.0.iter() {
            match p {
                Arg::Value => pos += 1,
                p => {
                    if pos != 0 {
                        if need_space {
                            write!(w, " ")?;
                        }
                        write!(w, "{}", pos)?;
                        need_space = true;
                        pos = 0;
                    }
                    if need_space {
                        write!(w, " ")?;
                    }
                    match p {
                        Arg::Value => unreachable!(),
                        Arg::Pack => write!(w, "⫶")?,
                        Arg::Key(id) => write!(w, "{}", &compiler.bintab[compiler.symtab[*id]])?,
                    }
                    need_space = true;
                }
            }
        }
        if pos != 0 {
            if need_space {
                write!(w, " ")?;
            }
            write!(w, "{}", pos)?;
        }
        Ok(())
    }
}

pub(crate) type PackTable = intern::Table<Pack, PackTag>;

pub(crate) struct UnpackTag;

pub(crate) type UnpackId = intern::Id<UnpackTag>;

#[derive(Hash, PartialEq, Eq, Debug, Clone, PartialOrd, Ord)]
pub(crate) enum UnpackKeyKind {
    Sym(sym::Id),
    Const(constant::Id),
}

#[derive(Hash, PartialEq, Eq, Debug, Clone, PartialOrd, Ord)]
pub(crate) struct UnpackKey {
    pub(crate) kind: UnpackKeyKind,
    pub(crate) default: Option<constant::Id>,
}

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub(crate) struct Unpack {
    required: usize,
    optional: Vec<constant::Id>,
    keys: Vec<UnpackKey>,
    variadic: Variadic,
}

impl Unpack {
    pub(crate) fn new(
        required: usize,
        optional: impl IntoIterator<Item = constant::Id>,
        keys: impl IntoIterator<Item = UnpackKey>,
        variadic: Variadic,
    ) -> Self {
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort();
        Self {
            required,
            optional: optional.into_iter().collect(),
            keys,
            variadic,
        }
    }

    pub(crate) fn required(&self) -> usize {
        self.required
    }

    pub(crate) fn optional(&self) -> impl DoubleEndedIterator<Item = &constant::Id> {
        self.optional.iter()
    }

    pub(crate) fn variadic(&self) -> Variadic {
        self.variadic
    }

    pub(crate) fn iter_keys(&self) -> impl DoubleEndedIterator<Item = &UnpackKey> {
        self.keys.iter()
    }

    pub(crate) fn len(&self) -> usize {
        self.required
            .strict_add(self.optional.len())
            .strict_add(self.keys.len())
            .strict_add(match self.variadic {
                Variadic::None | Variadic::Discard => 0,
                Variadic::Capture => 1,
            })
    }
}

#[cfg(feature = "debug")]
impl Unpack {
    pub fn dump(&self, compiler: &Compiler, w: &mut impl Write) -> fmt::Result {
        let mut need_space = false;
        if self.required != 0 {
            write!(w, "{}", self.required)?;
            need_space = true;
        }
        if !self.optional.is_empty() {
            if need_space {
                write!(w, " ")?;
            }
            write!(w, "(")?;
            for id in self.optional.iter() {
                compiler.consttab[*id].dump(compiler, w)?
            }
            write!(w, ")")?;
            need_space = true;
        }
        for id in self.keys.iter() {
            if need_space {
                write!(w, " ")?;
            }
            match &id.kind {
                UnpackKeyKind::Sym(sym) => {
                    write!(w, "{}", &compiler.bintab[compiler.symtab[*sym]])?;
                }
                UnpackKeyKind::Const(c) => {
                    compiler.consttab[*c].dump(compiler, w)?;
                }
            }
            if let Some(default) = &id.default {
                write!(w, "(")?;
                compiler.consttab[*default].dump(compiler, w)?;
                write!(w, ")")?;
            }
            need_space = true;
        }
        if self.variadic != Variadic::None {
            if need_space {
                write!(w, " ")?;
            }
            write!(w, "⫶")?;
        }
        Ok(())
    }
}

pub(crate) type UnpackTable = intern::Table<Unpack, UnpackTag>;
