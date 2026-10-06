//! Joins and widening of closed types, for flow state.
//!
//! A join is a union with subsumption: a member below another member is dropped.
//! Consistency with the dynamic type isn't antisymmetric, so a member containing
//! `Unknown` neither subsumes nor is subsumed, and `Unknown` itself absorbs the
//! join. A comparison that isn't proven keeps both members.
//!
//! Joining alone doesn't converge on a type that keeps growing, such as `x = [x]`
//! in a loop. [`Widening`] counts a flow variable's increases at a widening point
//! and widens in two stages: first to the least ancestor its union's members
//! share, then to `Unknown`. Sharing only `Value` widens to `Unknown` directly,
//! since a static top would make every later use a contradiction.
//!
//! Narrowing by a condition's relations is in [`super::narrow`].

use super::*;

/// Increases a variable's join may make at a widening point before each stage
pub(crate) const WIDENING_LIMIT: u32 = 3;

/// A flow variable's widening state at one widening point
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Widening {
    increases: u32,
}

impl Widening {
    /// Join `new` into `old`, widening once the join has grown too many times
    pub(crate) fn join(&mut self, solver: &Solver<'_>, old: TypeId, new: TypeId) -> TypeId {
        let joined = solver.lub(old, new);
        if joined == old {
            return old;
        }
        self.increases += 1;
        let unknown = solver.db.unknown();
        if self.increases > 2 * WIDENING_LIMIT {
            unknown
        } else if self.increases > WIDENING_LIMIT {
            solver.common_supertype(joined).unwrap_or(unknown)
        } else {
            joined
        }
    }
}

impl Solver<'_> {
    /// The join of two closed types: their union, without members that are below
    /// another member
    pub(crate) fn lub(&self, a: TypeId, b: TypeId) -> TypeId {
        let unknown = self.db.unknown();
        let below = |x: TypeId, y: TypeId| self.probe(x, y) == Ok(Status::Proven);
        let comparable = |member: &UnionMember| match *member {
            UnionMember::Type(ty) => (!self.contains_unknown(ty)).then_some(ty),
            _ => None,
        };
        let mut kept: Vec<UnionMember> = Vec::new();
        for member in self
            .union_members(a)
            .into_iter()
            .chain(self.union_members(b))
        {
            if member == UnionMember::Type(unknown) {
                return unknown;
            }
            if kept.contains(&member) {
                continue;
            }
            // A regular literal replaces its fresh twin, as normalization keeps it
            if let UnionMember::Type(ty) = member
                && let Some(twin) = kept.iter_mut().find(|kept| {
                    matches!(**kept, UnionMember::Type(kept) if kept != ty && self.db.regular(kept) == ty)
                })
            {
                *twin = member;
                continue;
            }
            if let Some(ty) = comparable(&member) {
                if kept.iter().filter_map(comparable).any(|k| below(ty, k)) {
                    continue;
                }
                kept.retain(|k| comparable(k).is_none_or(|k| !below(k, ty)));
            }
            kept.push(member);
        }
        self.db.intern(Type::Union(kept.into()))
    }

    /// The members of a closed type, as a union's
    pub(super) fn union_members(&self, ty: TypeId) -> Vec<UnionMember> {
        match self.db.ty(ty) {
            Type::Union(members) => members.to_vec(),
            _ => vec![UnionMember::Type(ty)],
        }
    }

    /// The least ancestor a union's members share, other than `Value`, with its
    /// arguments combined by variance. `None` when there is none, or it can't be
    /// found. A type other than a union is its own.
    pub(crate) fn common_supertype(&self, ty: TypeId) -> Option<TypeId> {
        let Type::Union(members) = self.db.ty(ty) else {
            return Some(ty);
        };
        let starts = members
            .iter()
            .map(|member| match *member {
                UnionMember::Type(ty) => self.start(ty).ok().flatten(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let mut candidates = Vec::new();
        self.preorder(
            starts.first()?.clone(),
            &mut HashSet::new(),
            0,
            &mut |visited| {
                if let Visited::Nominal(nominal) = visited
                    && !candidates.contains(&nominal.declaration)
                {
                    candidates.push(nominal.declaration);
                }
                Ok(None::<()>)
            },
        )
        .ok()?;
        let shared: Vec<_> = candidates
            .into_iter()
            .filter_map(|decl| self.shared(decl, &starts).map(|ty| (decl, ty)))
            .collect();
        // A shared ancestor above another isn't least; of those left, as a
        // diamond can leave several, the first in MRO order is taken
        let minimal = shared.iter().find(|&&(decl, _)| {
            shared.iter().all(|&(other, _)| {
                other == decl
                    || !matches!(
                        self.ancestor(self.bare(other), decl, &mut HashSet::new(), 0),
                        Ok(Some(_))
                    )
            })
        })?;
        Some(minimal.1)
    }

    /// Where a member's ancestors start: its nominal head, a literal's or a
    /// function's class, or a rigid's bound's
    pub(super) fn start(&self, ty: TypeId) -> Result<Option<Nominal>, Issue> {
        let mut term = self.closed(ty);
        for depth in 0.. {
            self.depth(depth)?;
            let view = match self.head(term)? {
                Head::Nominal(nominal) => return Ok(Some(nominal)),
                Head::Infer(_) | Head::Skolem(_) => return Ok(None),
                Head::Structural(view) => view,
            };
            let mut body = view.ty;
            while let Type::Quantified { body: inner, .. } = self.db.ty(body) {
                body = *inner;
            }
            let intrinsic = match self.db.ty(body) {
                Type::Function(_) => {
                    let Some(class) = self.db.func_class(view.ty) else {
                        return Ok(None);
                    };
                    term = self.view(class, view.environment);
                    continue;
                }
                Type::Literal(literal) => literal.intrinsic(),
                Type::Rigid { .. } => {
                    self.rigid(view.ty)?;
                    match self.rigid_bound(view.ty) {
                        Some(bound) => {
                            term = self.closed(bound);
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
            let Some(class) = self.db.intrinsic(intrinsic) else {
                return Ok(None);
            };
            term = self.closed(class);
        }
        unreachable!()
    }

    /// A declaration as the head of its own ancestor walk. Its arguments are
    /// irrelevant, since only whether it reaches another declaration is asked.
    fn bare(&self, decl: DeclId) -> Nominal {
        let arguments = match self.db.ty(self.db.declaration(decl).ty) {
            Type::Quantified { binders, .. } => binders
                .iter()
                .map(|binder| self.closed(self.db.unknown_of(binder.kind)))
                .collect(),
            _ => vec![],
        };
        Nominal {
            declaration: decl,
            arguments,
            environment: EnvironmentId(0),
        }
    }

    /// `decl` applied to the combination of each start's arguments for it, when
    /// every start reaches it and the arguments combine
    fn shared(&self, decl: DeclId, starts: &[Nominal]) -> Option<TypeId> {
        let binders: &[Binder] = match self.db.ty(self.db.declaration(decl).ty) {
            Type::Quantified { binders, .. } => binders,
            _ => &[],
        };
        if binders.iter().any(|b| b.binding != Binding::Positional) {
            return None;
        }
        let mut arguments: Vec<Vec<TypeId>> = vec![Vec::new(); binders.len()];
        for start in starts {
            let found = self
                .ancestor(start.clone(), decl, &mut HashSet::new(), 0)
                .ok()??;
            for (slot, term) in found.arguments.into_iter().enumerate() {
                arguments[slot].push(self.reify(term).ok()?);
            }
        }
        let base = self.db.intern(Type::Decl(decl));
        if binders.is_empty() {
            return Some(base);
        }
        let args = binders
            .iter()
            .zip(arguments)
            .map(|(binder, args)| self.combine(binder, &args).map(Argument::Positional))
            .collect::<Option<_>>()?;
        Some(self.db.intern(Type::Apply {
            base,
            args,
            kind: Kind::Type,
        }))
    }

    /// Combine the arguments a binder receives from each member, by its variance
    fn combine(&self, binder: &Binder, args: &[TypeId]) -> Option<TypeId> {
        let (&first, rest) = args.split_first()?;
        let same = rest.iter().all(|&arg| arg == first);
        if same || binder.kind == Kind::Schema {
            return same.then_some(first);
        }
        match binder.variance {
            Variance::Covariant => Some(
                rest.iter()
                    .fold(first, |joined, &arg| self.lub(joined, arg)),
            ),
            Variance::Contravariant => args.iter().copied().find(|&lowest| {
                args.iter()
                    .all(|&arg| self.probe(lowest, arg) == Ok(Status::Proven))
            }),
            Variance::Invariant => None,
        }
    }
}
