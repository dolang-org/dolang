//! Copying types between databases.
//!
//! A copy is rebuilt children first and interned in the destination, which ends
//! up holding only what was copied into it: the types, and the symbols and
//! declarations they refer to, with what those declarations refer to in turn.
//! Spans keep their units, which the destination allocates to match. Between
//! forks of one base, what the base holds keeps its IDs.

use std::{convert::Infallible, sync::Arc};

use rustc_hash::FxHashMap;

use super::{
    Argument, BinderSource, Binding, Database, DeclId, DeclSource, Declaration, Intrinsic, Literal,
    Member, MemberKey, Phase, Supertype, SymbolId, Type, TypeId,
};

/// Copies types from one database into another, with the symbols and
/// declarations they refer to. What it copied is memoized, so roots that share
/// a subterm copy it once.
#[cfg_attr(not(test), expect(dead_code, reason = "#1282 copies flow results"))]
pub(crate) struct Copier<'a> {
    from: &'a Database,
    into: &'a mut Database,
    /// The counts of types and symbols of the base both are forks of
    shared: Option<(usize, usize)>,
    /// The intrinsics `from` designates, by declaration
    intrinsics: FxHashMap<DeclId, Intrinsic>,
    types: FxHashMap<TypeId, TypeId>,
    symbols: FxHashMap<SymbolId, SymbolId>,
    decls: FxHashMap<DeclId, DeclId>,
    /// Declarations allocated but not yet populated, and their copies
    pending: Vec<(DeclId, DeclId)>,
}

impl Database {
    /// A copier from `from` into this database, whose units it allocates to match
    /// `from`'s, unless both are forks of one base. A copy that takes a new
    /// declaration needs this database not to be sealed.
    #[cfg_attr(not(test), expect(dead_code, reason = "#1282 copies flow results"))]
    pub(crate) fn copier<'a>(&'a mut self, from: &'a Database) -> Copier<'a> {
        let shared = match (&from.phase, &self.phase) {
            (Phase::Fork(base), Phase::Fork(other)) if Arc::ptr_eq(base, other) => {
                Some((base.types.len(), base.symbols.len()))
            }
            _ => None,
        };
        if shared.is_none() {
            while self.fixed.unit_count < from.fixed.unit_count {
                self.allocate_unit();
            }
        }
        let intrinsics = (Intrinsic::ALL.into_iter())
            .filter_map(|intrinsic| match *from.ty(from.intrinsic(intrinsic)?) {
                Type::Decl(decl) => Some((decl, intrinsic)),
                _ => None,
            })
            .collect();
        Copier {
            from,
            into: self,
            shared,
            intrinsics,
            types: FxHashMap::default(),
            symbols: FxHashMap::default(),
            decls: FxHashMap::default(),
            pending: Vec::new(),
        }
    }
}

impl Copier<'_> {
    /// A type's copy, with every declaration it reaches populated
    #[cfg_attr(not(test), expect(dead_code, reason = "#1282 copies flow results"))]
    pub(crate) fn ty(&mut self, ty: TypeId) -> TypeId {
        let copy = self.copy(ty);
        while let Some((decl, copy)) = self.pending.pop() {
            self.populate(decl, copy);
        }
        copy
    }

    fn copy(&mut self, ty: TypeId) -> TypeId {
        if self.shared.is_some_and(|(types, _)| ty.index() < types) {
            return ty;
        }
        if let Some(&copy) = self.types.get(&ty) {
            return copy;
        }
        let from = self.from;
        let Ok(mut node) = from
            .ty(ty)
            .map_children(|child, _| Ok::<_, Infallible>(self.copy(child)));
        match &mut node {
            Type::Unsupported { .. } => panic!("an unsupported stand-in can't be copied"),
            Type::Literal(Literal::Sym(name)) | Type::Fresh(Literal::Sym(name)) => {
                *name = self.symbol(*name)
            }
            Type::Decl(decl) | Type::Meta(decl) | Type::Rigid { decl, .. } => {
                *decl = self.decl(*decl)
            }
            Type::Apply { args, .. } => {
                for arg in args.iter_mut() {
                    if let Argument::Keyword(name, _) = arg {
                        *name = self.symbol(*name);
                    }
                }
            }
            Type::Quantified { binders, .. } => {
                for binder in binders.iter_mut() {
                    if let Binding::Keyword(name) = &mut binder.binding {
                        *name = self.symbol(*name);
                    }
                }
            }
            Type::Overloaded {
                function: Some(function),
                ..
            } => *function = self.decl(*function),
            _ => {}
        }
        let copy = self.into.intern(node);
        self.types.insert(ty, copy);
        copy
    }

    fn symbol(&mut self, name: SymbolId) -> SymbolId {
        if self
            .shared
            .is_some_and(|(_, symbols)| name.index() < symbols)
        {
            return name;
        }
        if let Some(&copy) = self.symbols.get(&name) {
            return copy;
        }
        let text = self.from.symbol(name);
        let copy = match self.from.is_fresh_symbol(name) {
            true => self.into.fresh_symbol(text),
            false => self.into.intern_symbol(text),
        };
        self.symbols.insert(name, copy);
        copy
    }

    /// A declaration's copy, allocated and designated as the intrinsic it is, but
    /// populated only once the type that reached it is copied, so that cycles
    /// through declarations don't recurse
    fn decl(&mut self, decl: DeclId) -> DeclId {
        if self.shared.is_some() {
            return decl;
        }
        if let Some(&copy) = self.decls.get(&decl) {
            return copy;
        }
        let copy = self.into.allocate();
        self.decls.insert(decl, copy);
        if let Some(&intrinsic) = self.intrinsics.get(&decl) {
            let ty = self.into.intern(Type::Decl(copy));
            self.into.set_intrinsic(intrinsic, ty);
        }
        self.pending.push((decl, copy));
        copy
    }

    fn populate(&mut self, decl: DeclId, copy: DeclId) {
        let from = self.from;
        let declaration = from.declaration(decl);
        let source = DeclSource {
            name: declaration.source.name.map(|name| self.symbol(name)),
            ..declaration.source.clone()
        };
        let binders = (declaration.binders.iter())
            .map(|binder| BinderSource {
                name: self.symbol(binder.name),
                ..binder.clone()
            })
            .collect();
        let supertypes = (declaration.supertypes.iter())
            .map(|supertype| Supertype {
                ty: self.copy(supertype.ty),
                ..*supertype
            })
            .collect();
        let members = (declaration.members.iter())
            .map(|(key, member)| {
                let key = MemberKey {
                    name: self.symbol(key.name),
                    ..*key
                };
                let member = match *member {
                    Member::Field { ty, scope, public } => Member::Field {
                        ty: self.copy(ty),
                        scope,
                        public,
                    },
                    Member::Method {
                        decl,
                        scope,
                        public,
                    } => Member::Method {
                        decl: self.decl(decl),
                        scope,
                        public,
                    },
                    Member::Property {
                        getter,
                        setter,
                        scope,
                        public,
                    } => Member::Property {
                        getter: getter.map(|decl| self.decl(decl)),
                        setter: setter.map(|decl| self.decl(decl)),
                        scope,
                        public,
                    },
                    Member::Decorated {
                        decl,
                        scope,
                        public,
                    } => Member::Decorated {
                        decl: self.decl(decl),
                        scope,
                        public,
                    },
                };
                (key, member)
            })
            .collect();
        let ty = self.copy(declaration.ty);
        self.into.populate(
            copy,
            Declaration {
                source,
                ty,
                binders,
                supertypes,
                members,
            },
        );
        let overloads = from.overloads(decl);
        if !overloads.is_empty() {
            let overloads = overloads.iter().map(|&decl| self.decl(decl)).collect();
            self.into.set_overloads(copy, overloads);
        }
    }
}
