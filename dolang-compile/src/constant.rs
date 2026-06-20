use std::{
    hash::{Hash, Hasher},
    mem,
};

use dolang_util::intern::{self, BinId, StrId};

use crate::sym;

#[cfg(feature = "debug")]
use super::Compiler;

#[cfg(feature = "debug")]
use std::fmt::{self, Write};

pub(crate) struct Tag;

pub(crate) type Id = intern::Id<Tag>;
pub(crate) type Int = i128;

#[derive(Clone)]
pub(crate) enum Const {
    Nil,
    Int(Int),
    VerbatimInt(Int, StrId),
    F64(f64),
    VerbatimF64(f64, StrId),
    Bool(bool),
    Str(StrId),
    Sym(sym::Id),
    Bin(BinId),
}

impl Hash for Const {
    fn hash<H: Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state);
        match self {
            Const::Nil => 0u8.hash(state),
            Const::Int(v) => v.hash(state),
            Const::F64(v) => {
                if v.is_nan() {
                    f64::NAN.to_bits().hash(state)
                } else {
                    v.to_bits().hash(state)
                }
            }
            Const::Bool(v) => v.hash(state),
            Const::Str(id) | Const::VerbatimInt(_, id) | Const::VerbatimF64(_, id) => {
                id.hash(state)
            }
            Const::Sym(id) => id.hash(state),
            Const::Bin(id) => id.hash(state),
        }
    }
}

impl PartialEq for Const {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(l), Self::Int(r)) => l == r,
            (Self::VerbatimInt(_, l), Self::VerbatimInt(_, r)) => l == r,
            // We need to be able to locate NaN if it really ends up in the table
            (Self::F64(l), Self::F64(r)) => l.is_nan() && r.is_nan() || l == r,
            (Self::Bool(l), Self::Bool(r)) => l == r,
            (Self::Str(l), Self::Str(r)) => l == r,
            (Self::Nil, Self::Nil) => true,
            (Self::Bin(l), Self::Bin(r)) => l == r,
            _ => false,
        }
    }
}

impl Eq for Const {}

#[cfg(feature = "debug")]
impl Const {
    pub(crate) fn dump(&self, compiler: &Compiler, w: &mut impl Write) -> fmt::Result {
        match self {
            Const::Nil => write!(w, "nil"),
            Const::Int(v) => write!(w, "{}", v),
            Const::VerbatimInt(v, id) => write!(w, "{}«{}»", v, &compiler.bintab[*id]),
            Const::F64(v) => write!(w, "{}", v),
            Const::VerbatimF64(v, id) => write!(w, "{}«{}»", v, &compiler.bintab[*id]),
            Const::Bool(v) => write!(w, "{}", v),
            Const::Str(id) => write!(w, "{:?}", &compiler.bintab[*id]),
            Const::Bin(id) => write!(w, "{:?}", &compiler.bintab[*id]),
            Const::Sym(id) => write!(w, ":{}:", &compiler.bintab[compiler.symtab[*id]]),
        }
    }
}

pub(crate) type Table = intern::Table<Const, Tag>;

pub(crate) trait ConstantExt {
    fn int(&mut self, value: Int) -> Id;
    fn verbatim_int(&mut self, value: Int, text: StrId) -> Id;
    fn f64(&mut self, value: f64) -> Id;
    fn verbatim_f64(&mut self, value: f64, text: StrId) -> Id;
    fn str(&mut self, value: StrId) -> Id;
    fn bool(&mut self, value: bool) -> Id;
    fn nil(&mut self) -> Id;
    fn sym(&mut self, value: sym::Id) -> Id;
    fn bin(&mut self, value: BinId) -> Id;
}

impl ConstantExt for Table {
    fn int(&mut self, value: Int) -> Id {
        self.id(&Const::Int(value))
    }

    fn verbatim_int(&mut self, value: Int, text: StrId) -> Id {
        self.id(&Const::VerbatimInt(value, text))
    }

    fn f64(&mut self, value: f64) -> Id {
        self.id(&Const::F64(value))
    }

    fn verbatim_f64(&mut self, value: f64, text: StrId) -> Id {
        self.id(&Const::VerbatimF64(value, text))
    }

    fn str(&mut self, value: StrId) -> Id {
        self.id(&Const::Str(value))
    }

    fn bool(&mut self, value: bool) -> Id {
        self.id(&Const::Bool(value))
    }

    fn sym(&mut self, value: sym::Id) -> Id {
        self.id(&Const::Sym(value))
    }

    fn nil(&mut self) -> Id {
        self.id(&Const::Nil)
    }

    fn bin(&mut self, value: BinId) -> Id {
        self.id(&Const::Bin(value))
    }
}
