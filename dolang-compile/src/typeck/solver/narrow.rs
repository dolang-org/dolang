//! Narrowing of closed types by the relations a condition establishes.
//!
//! A variable's flow type is narrowed member by member against a class `C`, from
//! `Type[C]`, or a literal. The result is sound: it's always above the member's
//! true intersection with the target. An intersection that can't be represented
//! is over-approximated by the member or by `C`, and a reach that can't be proven
//! gives the conservative outcome: a member is kept by a negative relation and
//! becomes `C` by a positive one. A member becomes `C` applied to `Unknown`
//! arguments, or to the member's own where they carry down soundly. An empty
//! result is bottom, making the edge unreachable.
//!
//! A class's `(==)` may be user-defined, so a literal comparison strips only
//! other literals and never reduces a class to the literal. Only a `Nil` or a
//! `Bool` member, whose values are all literals, loses the literal it's unequal
//! to.

use super::*;
use crate::typeck::cfg::Relation as Narrowing;

/// What a variable is narrowed against, as flow analysis evaluates an `Against`
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Used by flow analysis (#736)
pub(crate) enum Target {
    /// The class `C` of `Type[C]`, possibly generic and unapplied
    Class(DeclId),
    /// A literal type
    Literal(TypeId),
}

/// A union member, as narrowing sees it
enum Member {
    Unknown,
    Top,
    Literal(Literal, Option<Nominal>),
    /// Its class, or a bound rigid's bound's
    Class(Nominal),
    /// An unbounded rigid, or a type whose class can't be found
    Opaque,
}

#[allow(dead_code)] // Used by flow analysis (#736)
impl Solver<'_> {
    /// Narrow a closed type by `relation` against `target`, or by its negation
    pub(crate) fn narrow(
        &self,
        ty: TypeId,
        relation: Narrowing,
        negated: bool,
        target: Target,
    ) -> TypeId {
        let literal = match target {
            Target::Class(_) => None,
            Target::Literal(literal) => match self.db.ty(literal) {
                Type::Literal(literal) => Some(literal),
                _ => return ty,
            },
        };
        let mut result = self.db.bottom();
        for member in self.union_members(ty) {
            // A pack's members are unknown, so it's kept
            let member = match member {
                UnionMember::Type(member) => member,
                UnionMember::Expand(_) => {
                    let pack = self.db.intern(Type::Union(vec![member].into()));
                    result = self.lub(result, pack);
                    continue;
                }
            };
            let narrowed = match (target, literal) {
                (Target::Class(class), _) => self.narrow_class(member, relation, negated, class),
                (_, Some(literal)) => {
                    assert_eq!(relation, Narrowing::Exact, "narrowing below a literal");
                    self.narrow_literal(member, negated, literal)
                }
                (Target::Literal(_), None) => unreachable!(),
            };
            if let Some(narrowed) = narrowed {
                result = self.lub(result, narrowed);
            }
        }
        result
    }

    /// A member narrowed against a class, or `None` when it's dropped
    fn narrow_class(
        &self,
        kept: TypeId,
        relation: Narrowing,
        negated: bool,
        class: DeclId,
    ) -> Option<TypeId> {
        let seen = self.classify(kept);
        let whole = || self.apply_unknown(class);
        match (relation, negated) {
            (Narrowing::Upper, false) => match seen {
                Member::Class(nominal) => match self.reaches(&nominal, class) {
                    Some(true) => Some(kept),
                    Some(false) if self.disjoint(&nominal, class) => None,
                    _ => Some(self.below(class, &nominal)),
                },
                Member::Literal(_, Some(nominal)) => match self.reaches(&nominal, class) {
                    Some(true) => Some(kept),
                    Some(false) => None,
                    None => Some(whole()),
                },
                Member::Literal(_, None) => Some(kept),
                Member::Unknown | Member::Top | Member::Opaque => Some(whole()),
            },
            (Narrowing::Upper, true) => match seen {
                Member::Class(nominal) | Member::Literal(_, Some(nominal))
                    if self.reaches(&nominal, class) == Some(true) =>
                {
                    None
                }
                _ => Some(kept),
            },
            (Narrowing::Exact, false) => match seen {
                Member::Class(nominal) if nominal.declaration == class => Some(kept),
                Member::Class(nominal) => match self.descent(class, nominal.declaration) {
                    Ok(Some(_)) => Some(self.below(class, &nominal)),
                    // An instance of exactly `C` is a member's only if `C` reaches it
                    Ok(None) => None,
                    Err(_) => Some(whole()),
                },
                Member::Literal(_, Some(nominal)) => (nominal.declaration == class).then_some(kept),
                Member::Literal(_, None) => Some(kept),
                Member::Unknown | Member::Top | Member::Opaque => Some(whole()),
            },
            (Narrowing::Exact, true) => match seen {
                // A literal's class is exactly its intrinsic
                Member::Literal(_, Some(nominal)) if nominal.declaration == class => None,
                _ => Some(kept),
            },
        }
    }

    /// A member narrowed by equality with a literal, or `None` when it's dropped
    fn narrow_literal(&self, kept: TypeId, negated: bool, target: &Literal) -> Option<TypeId> {
        match self.classify(kept) {
            Member::Literal(literal, _) => ((literal == *target) != negated).then_some(kept),
            Member::Class(nominal) if negated => {
                let is = |intrinsic| self.intrinsic_decl(intrinsic) == Some(nominal.declaration);
                match target {
                    // `Nil` and `Bool` hold only literals, with built-in equality
                    Literal::Nil if is(Intrinsic::Nil) => None,
                    &Literal::Bool(value) if is(Intrinsic::Bool) => {
                        Some(self.db.intern(Type::Literal(Literal::Bool(!value))))
                    }
                    _ => Some(kept),
                }
            }
            _ => Some(kept),
        }
    }

    fn classify(&self, ty: TypeId) -> Member {
        match self.db.ty(ty) {
            Type::Unknown(_) => return Member::Unknown,
            Type::Top => return Member::Top,
            Type::Literal(literal) => {
                return Member::Literal(literal.clone(), self.start(ty).ok().flatten());
            }
            _ => {}
        }
        match self.start(ty) {
            Ok(Some(nominal)) => Member::Class(nominal),
            _ => Member::Opaque,
        }
    }

    /// Whether a class reaches `target`, or `None` when that can't be proven
    fn reaches(&self, nominal: &Nominal, target: DeclId) -> Option<bool> {
        self.ancestor(nominal.clone(), target, &mut HashSet::new(), 0)
            .ok()
            .map(|found| found.is_some())
    }

    /// Whether a class that doesn't reach `class` provably has no instance of it:
    /// it's a literal's class that `class` doesn't reach either
    fn disjoint(&self, nominal: &Nominal, class: DeclId) -> bool {
        [
            Intrinsic::Nil,
            Intrinsic::Bool,
            Intrinsic::Int,
            Intrinsic::Str,
            Intrinsic::Sym,
        ]
        .into_iter()
        .any(|intrinsic| self.intrinsic_decl(intrinsic) == Some(nominal.declaration))
            && matches!(self.descent(class, nominal.declaration), Ok(None))
    }

    fn intrinsic_decl(&self, intrinsic: Intrinsic) -> Option<DeclId> {
        match self.db.ty(self.db.intrinsic(intrinsic)?) {
            &Type::Decl(decl) => Some(decl),
            _ => None,
        }
    }

    /// `class` applied to its own rigids, walked to `decl`: the arguments `decl`
    /// has there, or `None` when `class` doesn't reach it
    fn descent(&self, class: DeclId, decl: DeclId) -> Result<Option<Vec<TypeId>>, Issue> {
        let mut nested = self.nested()?;
        let environment = nested.rigid_environment(class);
        let arguments = self
            .db
            .rigids(class)
            .into_iter()
            .map(|ty| nested.closed(ty))
            .collect();
        let start = Nominal {
            declaration: class,
            arguments,
            environment,
        };
        let result = nested
            .ancestor(start, decl, &mut HashSet::new(), 0)
            .and_then(|found| {
                found
                    .map(|found| {
                        found
                            .arguments
                            .into_iter()
                            .map(|term| nested.reify(term))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()
                    .map_err(Issue::from)
            });
        self.work.set(self.work.get() + nested.work.get());
        result
    }

    /// `class`, which a value of `member` is an instance of, with the arguments
    /// that carry down from `member`, and `Unknown` for the rest
    fn below(&self, class: DeclId, member: &Nominal) -> TypeId {
        let Some(binders) = self.binders(class) else {
            return self.db.intern(Type::Decl(class));
        };
        let (Ok(Some(descent)), Ok(arguments), Some(member_binders)) = (
            self.descent(class, member.declaration),
            member
                .arguments
                .iter()
                .map(|&term| self.reify(term))
                .collect::<Result<Vec<_>, _>>(),
            self.binders(member.declaration),
        ) else {
            return self.apply_unknown(class);
        };
        let rigids = self.db.rigids(class);
        let args = binders
            .iter()
            .zip(rigids)
            .map(|(binder, rigid)| {
                // A value of `C[x]` is a `member[a]`; `C[a]` is above `C[x]` if
                // the member's binder is invariant or both vary alike
                let mut carried = descent
                    .iter()
                    .zip(&arguments)
                    .zip(member_binders.iter())
                    .filter(|&((&found, _), _)| found == rigid)
                    .map(|((_, &argument), member_binder)| {
                        (member_binder.variance == Variance::Invariant
                            || member_binder.variance == binder.variance)
                            .then_some(argument)
                    });
                let first = carried.next().flatten();
                let argument = first
                    .filter(|&first| carried.all(|other| other == Some(first)))
                    .unwrap_or_else(|| self.db.unknown_of(binder.kind));
                Argument::Positional(argument)
            })
            .collect();
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args,
            kind: Kind::Type,
        })
    }

    /// `class` applied to `Unknown` for each binder
    fn apply_unknown(&self, class: DeclId) -> TypeId {
        let base = self.db.intern(Type::Decl(class));
        let Some(binders) = self.binders(class) else {
            return base;
        };
        let args = binders
            .iter()
            .map(|binder| Argument::Positional(self.db.unknown_of(binder.kind)))
            .collect();
        self.db.intern(Type::Apply {
            base,
            args,
            kind: Kind::Type,
        })
    }

    /// A generic declaration's binders
    fn binders(&self, decl: DeclId) -> Option<&[Binder]> {
        match self.db.ty(self.db.declaration(decl).ty) {
            Type::Quantified { binders, .. } if !binders.is_empty() => Some(binders),
            _ => None,
        }
    }
}
