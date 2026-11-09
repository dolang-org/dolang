//! Whether what a public declaration's decorators settled to can be quoted: written
//! into a typelib, for an importer to read as given.
//!
//! A typelib holds a declaration's value to what could be written as a def's
//! signatures, or as a type expression. So at the top of a value, a function may
//! have binders, as a def's signature does, though a type expression can't: a
//! generic function, and each signature of an overloaded one. So may a class
//! object, which is its class's. Anywhere else, the value must be what a type
//! expression writes: it
//! has no quantifier, overloaded function or class object, no type parameter of a
//! declaration being checked, and names only what its unit exports.
//!
//! What a typelib can't write, an importer reads as `Unknown`.

use super::{Tables, sig};
use crate::typeck::r#type::{Argument, Database, DeclId, DeclKind, Type, TypeId};

/// Why a settled value can't be quoted
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unquotable {
    /// It names a declaration its unit doesn't export
    Private(DeclId),
    /// It has a part no type expression writes, as this says
    Form(&'static str),
}

/// What a settled value is that can't be quoted, if anything
pub(crate) fn unquotable(db: &Database, tables: &Tables<'_>, ty: TypeId) -> Option<Unquotable> {
    quoted(db, &|decl| sig::exported(tables, decl), ty)
}

/// [`unquotable`], with what another unit can name given by `exported`
fn quoted(db: &Database, exported: &dyn Fn(DeclId) -> bool, ty: TypeId) -> Option<Unquotable> {
    Quote { db, exported }.top(ty).err()
}

/// A rigid's reason, for one that escaped its declaration's check
pub(crate) const ESCAPED: Unquotable = Unquotable::Form("a type parameter outside its declaration");

struct Quote<'a> {
    db: &'a Database,
    exported: &'a dyn Fn(DeclId) -> bool,
}

impl Quote<'_> {
    /// A whole value, which may be a def's signatures or a class object
    fn top(&self, ty: TypeId) -> Result<(), Unquotable> {
        match self.db.ty(ty) {
            Type::Overloaded {
                overloads,
                implementation,
                ..
            } => {
                for &signature in overloads.iter().chain(implementation) {
                    self.signature(signature)?;
                }
                Ok(())
            }
            Type::Quantified { binders, body } => {
                for binder in binders.iter() {
                    for ty in binder.bound.into_iter().chain(binder.default) {
                        self.nested(ty)?;
                    }
                }
                match self.db.ty(*body) {
                    Type::Function(_) => self.children(*body, false),
                    _ if self.db.meta_of(*body).is_some() => self.class_object(*body),
                    _ => Err(Unquotable::Form("a generic value that isn't a function")),
                }
            }
            // An overloaded function, quoted as its signatures
            &Type::Decl(decl) if self.db.declaration(decl).source.kind == DeclKind::Function => {
                let signatures = self.db.overloads(decl).iter().copied();
                for signature in signatures.chain(self.db.implementation(decl)) {
                    self.signature(self.db.declaration(signature).ty)?;
                }
                Ok(())
            }
            _ if self.db.meta_of(ty).is_some() => self.class_object(ty),
            _ => self.nested(ty),
        }
    }

    /// One of a def's signatures, which may have binders
    fn signature(&self, ty: TypeId) -> Result<(), Unquotable> {
        match self.db.ty(ty) {
            Type::Quantified { body, .. } if matches!(self.db.ty(*body), Type::Function(_)) => {
                self.top(ty)
            }
            Type::Function(_) => self.children(ty, false),
            _ => self.nested(ty),
        }
    }

    /// A class's object, by its class, applied to arguments that are types
    fn class_object(&self, ty: TypeId) -> Result<(), Unquotable> {
        let (class, _) = self.db.meta_of(ty).expect("a class object");
        self.named(class)?;
        match self.db.ty(ty) {
            Type::Apply { args, .. } => {
                for arg in args.iter() {
                    self.argument(arg)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn argument(&self, arg: &Argument) -> Result<(), Unquotable> {
        let (Argument::Positional(ty) | Argument::Keyword(_, ty) | Argument::Expand(ty)) = *arg;
        self.nested_at(ty, true)
    }

    /// A declaration a quote names, which another unit must be able to name
    fn named(&self, decl: DeclId) -> Result<(), Unquotable> {
        match (self.exported)(decl) {
            true => Ok(()),
            false => Err(Unquotable::Private(decl)),
        }
    }

    /// Part of a type expression
    fn nested(&self, ty: TypeId) -> Result<(), Unquotable> {
        self.nested_at(ty, false)
    }

    /// Part of a type expression, within a type argument if `argument`, which a
    /// quantified type can't be, though it can be a function's parameter or result
    fn nested_at(&self, ty: TypeId, argument: bool) -> Result<(), Unquotable> {
        match *self.db.ty(ty) {
            Type::Rigid { .. } => Err(ESCAPED),
            Type::Quantified { body, .. } if matches!(self.db.ty(body), Type::Function(_)) => {
                match argument {
                    true => Err(Unquotable::Form("a generic function as a type argument")),
                    false => self.children(ty, false),
                }
            }
            Type::Quantified { .. } => Err(Unquotable::Form(
                "a generic value that isn't a function inside another type",
            )),
            Type::Function(_) => self.children(ty, false),
            Type::Apply { base, ref args, .. } => {
                self.nested_at(base, false)?;
                for arg in args.iter() {
                    let (Argument::Positional(arg)
                    | Argument::Keyword(_, arg)
                    | Argument::Expand(arg)) = *arg;
                    self.nested_at(arg, true)?;
                }
                Ok(())
            }
            Type::Overloaded { .. } => Err(Unquotable::Form(
                "an overloaded function inside another type",
            )),
            Type::Meta(_) => Err(Unquotable::Form("a class object inside another type")),
            Type::Unsupported { .. } => Err(Unquotable::Form("a type the checker can't represent")),
            Type::Decl(decl) => match self.db.declaration(decl).source.kind {
                DeclKind::Function => Err(Unquotable::Form(
                    "an overloaded function inside another type",
                )),
                _ => self.named(decl),
            },
            _ => self.children(ty, argument),
        }
    }

    fn children(&self, ty: TypeId, argument: bool) -> Result<(), Unquotable> {
        let mut result = Ok(());
        self.db.ty(ty).visit_children(|child, _| {
            if result.is_ok() {
                result = self.nested_at(child, argument);
            }
        });
        result
    }
}

#[cfg(test)]
mod tests;
