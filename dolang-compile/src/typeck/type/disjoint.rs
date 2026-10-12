//! Disjoint classes, after Python's PEP 800.
//!
//! Every class is disjoint unless decorated `#[mixable]`, and `Value` is
//! disjoint. A mixable class takes the position of its nearest disjoint
//! ancestor: the one, among its runtime supertypes' nearest, that reaches all
//! the others. A class whose runtime supertypes' nearest disjoint ancestors
//! have no such one is incoherent, which the conformance pass reports. The
//! disjoint classes of coherent declarations form a tree under `Value`, so two
//! classes whose nearest disjoint ancestors are unrelated in it share no
//! subclass, and no instance. Protocols have no position.
//!
//! Only declared supertypes count, type arguments aside. A typelib's
//! declarations are trusted to be coherent; one that isn't makes conclusions
//! unsound, but never fails a walk.

use rustc_hash::{FxHashMap, FxHashSet};

use super::{Database, DeclId, DeclKind, Intrinsic, Type, TypeId};

/// A nearest disjoint ancestor, with `None` standing for `Value`
pub(crate) type Ancestor = Option<DeclId>;

/// Why a declaration has no nearest disjoint ancestor
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoAncestor {
    /// Two of its runtime supertypes' ancestors, neither reaching the other
    Incoherent(Ancestor, Ancestor),
    /// It is a protocol or isn't a class, a supertype isn't a declaration, or
    /// supertypes cycle
    Unknown,
}

type Memo = FxHashMap<DeclId, Result<Ancestor, NoAncestor>>;

impl Database {
    /// The nearest disjoint ancestor a class's runtime supertypes share, which
    /// is a mixable class's own
    pub(crate) fn supertypes_ancestor(&self, decl: DeclId) -> Result<Ancestor, NoAncestor> {
        self.shared(decl, &mut Memo::default(), &mut FxHashSet::default())
    }

    /// Whether two classes have no instance in common: each has a nearest
    /// disjoint ancestor, and neither reaches the other's
    pub(crate) fn disjoint_apart(&self, a: DeclId, b: DeclId) -> bool {
        let nearest = |decl| self.nearest(decl, &mut Memo::default(), &mut FxHashSet::default());
        let (Ok(Some(a)), Ok(Some(b))) = (nearest(a), nearest(b)) else {
            return false;
        };
        a != b && !self.reaches_decl(a, b) && !self.reaches_decl(b, a)
    }

    /// A class's nearest disjoint ancestor: itself unless it's mixable
    fn nearest(
        &self,
        decl: DeclId,
        memo: &mut Memo,
        path: &mut FxHashSet<DeclId>,
    ) -> Result<Ancestor, NoAncestor> {
        if Some(decl) == self.intrinsic_decl(Intrinsic::Value) {
            return Ok(None);
        }
        let source = &self.declaration(decl).source;
        match source.kind {
            DeclKind::Class if source.is_mixable => self.shared(decl, memo, path),
            DeclKind::Class => Ok(Some(decl)),
            _ => Err(NoAncestor::Unknown),
        }
    }

    fn shared(
        &self,
        decl: DeclId,
        memo: &mut Memo,
        path: &mut FxHashSet<DeclId>,
    ) -> Result<Ancestor, NoAncestor> {
        if let Some(&found) = memo.get(&decl) {
            return found;
        }
        if !path.insert(decl) {
            return Err(NoAncestor::Unknown);
        }
        let found = self.shared_uncached(decl, memo, path);
        path.remove(&decl);
        memo.insert(decl, found);
        found
    }

    fn shared_uncached(
        &self,
        decl: DeclId,
        memo: &mut Memo,
        path: &mut FxHashSet<DeclId>,
    ) -> Result<Ancestor, NoAncestor> {
        let declaration = self.declaration(decl);
        if declaration.source.kind != DeclKind::Class {
            return Err(NoAncestor::Unknown);
        }
        let mut candidates = Vec::with_capacity(declaration.supertypes.len());
        for supertype in (declaration.supertypes.iter()).filter(|supertype| supertype.runtime) {
            let ancestor = match self.supertype_decl(supertype.ty)? {
                // A supertype's incoherence is reported where it's declared
                Some(head) => (self.nearest(head, memo, path)).map_err(|_| NoAncestor::Unknown)?,
                None => None,
            };
            if !candidates.contains(&ancestor) {
                candidates.push(ancestor);
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        let reaches = |from: Ancestor, to: Ancestor| match (from, to) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(from), Some(to)) => self.reaches_decl(from, to),
        };
        let Some(&nearest) = (candidates.iter())
            .find(|&&candidate| candidates.iter().all(|&other| reaches(candidate, other)))
        else {
            let (first, second) = (candidates.iter())
                .enumerate()
                .find_map(|(index, &first)| {
                    candidates[index + 1..]
                        .iter()
                        .find(|&&second| !reaches(first, second) && !reaches(second, first))
                        .map(|&second| (first, second))
                })
                .unwrap_or((candidates[0], candidates[1]));
            return Err(NoAncestor::Incoherent(first, second));
        };
        Ok(nearest)
    }

    /// The declaration a supertype applies, `None` for `Value`
    fn supertype_decl(&self, ty: TypeId) -> Result<Option<DeclId>, NoAncestor> {
        if ty == self.top() {
            return Ok(None);
        }
        let head = match *self.ty(ty) {
            Type::Apply { base, .. } => base,
            _ => ty,
        };
        match *self.ty(head) {
            Type::Decl(decl) if Some(decl) == self.intrinsic_decl(Intrinsic::Value) => Ok(None),
            Type::Decl(decl) => Ok(Some(decl)),
            _ => Err(NoAncestor::Unknown),
        }
    }

    /// Whether `from` names `to` among its supertypes, directly or through
    /// theirs, type arguments aside
    fn reaches_decl(&self, from: DeclId, to: DeclId) -> bool {
        let mut seen = FxHashSet::default();
        let mut stack = vec![from];
        while let Some(decl) = stack.pop() {
            if decl == to {
                return true;
            }
            if !seen.insert(decl) {
                continue;
            }
            let supertypes = &self.declaration(decl).supertypes;
            stack.extend(
                (supertypes.iter())
                    .filter_map(|supertype| self.supertype_decl(supertype.ty).ok().flatten()),
            );
        }
        false
    }

    /// The declaration an intrinsic is, when it is one
    pub(crate) fn intrinsic_decl(&self, intrinsic: Intrinsic) -> Option<DeclId> {
        match *self.ty(self.intrinsic(intrinsic)?) {
            Type::Decl(decl) => Some(decl),
            _ => None,
        }
    }
}
