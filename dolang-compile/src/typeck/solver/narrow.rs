//! Narrowing of closed types by the relations a condition establishes.
//!
//! A variable's flow type is narrowed member by member against a class `C`, from
//! `Type[C]`, or a literal. The result is sound: it's always above the member's
//! true intersection with the target. An intersection that can't be represented
//! is over-approximated by the member or by `C`, and a reach that can't be proven
//! gives the conservative outcome: a member is kept by a negative relation and
//! becomes `C` by a positive one. A member becomes `C` applied to the member's
//! own arguments where they carry down soundly. A gradual unit's solver gives
//! the rest `Unknown`. A strict unit's gives a covariant binder its bound, of
//! either kind, and a contravariant type binder bottom, and keeps the member
//! where another binder leaves no sound argument. An empty result is bottom, making the edge
//! unreachable.
//!
//! Narrowing against `Func` keeps each function and each class that reaches
//! `Func`, and makes any other class the gradual function type: a subclass may
//! reach `Func`, so no class is dropped.
//!
//! A class's `(==)` may be user-defined, so a literal comparison strips only
//! other literals and never reduces a class to the literal. Only a `Nil` or a
//! `Bool` member, whose values are all literals, loses the literal it's unequal
//! to.

use super::*;
use crate::typeck::cfg::Relation as Narrowing;

/// What a variable is narrowed against, as flow analysis evaluates an `Against`
#[derive(Clone, Copy, Debug)]
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
            Target::Literal(literal) => match self.db.literal(literal) {
                Some(literal) => Some(literal),
                None => return ty,
            },
        };
        // A transparent alias can hide the alternatives whose class test
        // preserves their own type arguments, as FmtSegment[V] does.
        let exposed = match self.head(self.closed(ty)) {
            Ok(Head::Structural(view)) if matches!(self.db.ty(view.ty), Type::Union(_)) => {
                self.reify(Term::View(view)).unwrap_or(ty)
            }
            _ => ty,
        };
        let mut result = self.db.bottom();
        for member in self.union_members(exposed) {
            // A projection's members are unknown, so it's kept
            let member = match member {
                UnionMember::Type(member) => member,
                _ => {
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
        // An `Unknown` member stays dynamic as `C`
        let unknown = matches!(seen, Member::Unknown);
        let whole = || match unknown {
            true => self.apply_unknown(class),
            false => self.whole(class, kept),
        };
        match (relation, negated) {
            (Narrowing::Upper, false) => match seen {
                Member::Class(nominal) => match self.reaches(&nominal, class) {
                    Some(true) => Some(kept),
                    Some(false) if self.disjoint(&nominal, class) => None,
                    _ => Some(self.below(class, &nominal, kept)),
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
                    Ok(Some(_)) => Some(self.below(class, &nominal, kept)),
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
            Type::Literal(literal) | Type::Fresh(literal) => {
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
    /// its values are all literals, as `Nil`'s and `Bool`'s are, and `class`
    /// doesn't reach it either. Another class may have a subclass that is also a
    /// `class`.
    fn disjoint(&self, nominal: &Nominal, class: DeclId) -> bool {
        [Intrinsic::Nil, Intrinsic::Bool]
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

    /// `class` as a whole, for a member `kept` whose class says nothing of it.
    /// A gradual solver applies it to `Unknown`; a strict one approximates its
    /// arguments soundly, or keeps the member when it can't.
    fn whole(&self, class: DeclId, kept: TypeId) -> TypeId {
        if self.gradual {
            return self.apply_unknown(class);
        }
        self.approximate(class, &|_| None).unwrap_or(kept)
    }

    /// `class` applied to `carried`'s argument for each binder that has one, and
    /// for the rest a [sound argument](Solver::sound_argument); `None` when a
    /// binder has neither
    fn approximate(
        &self,
        class: DeclId,
        carried: &dyn Fn(usize) -> Option<TypeId>,
    ) -> Option<TypeId> {
        let base = self.db.intern(Type::Decl(class));
        let Some(binders) = self.binders(class) else {
            return Some(base);
        };
        let args = (binders.iter().enumerate())
            .map(|(slot, binder)| {
                carried(slot)
                    .or_else(|| self.sound_argument(binder, binders))
                    .map(Argument::Positional)
            })
            .collect::<Option<_>>()?;
        Some(self.db.intern(Type::Apply {
            base,
            args,
            kind: Kind::Type,
        }))
    }

    /// An argument every instance's own is below or above, as the binder varies,
    /// written without `Unknown`: a covariant binder's bound, or without one `Value`
    /// or the open schema, and a contravariant type binder's bottom. A bound that
    /// needs the other arguments isn't one, and there's no bottom schema.
    fn sound_argument(&self, binder: &Binder, binders: &[Binder]) -> Option<TypeId> {
        match (binder.variance, binder.kind) {
            (Variance::Covariant, _) => {
                let unknowns: Vec<_> = (binders.iter())
                    .map(|binder| self.db.unknown_of(binder.kind))
                    .collect();
                let bound = match (self.db.binder_bound(binder, &unknowns), binder.kind) {
                    (Some(bound), _) => bound,
                    (None, Kind::Type) => return Some(self.db.top()),
                    (None, Kind::Schema) => self.db.rest_shape(Rest::All),
                };
                (!self.contains_unknown(bound)).then_some(bound)
            }
            (Variance::Contravariant, Kind::Type) => Some(self.db.bottom()),
            (Variance::Contravariant, Kind::Schema) | (Variance::Invariant, _) => None,
        }
    }

    /// `class`, which a value of `member` is an instance of, with the arguments
    /// that carry down from `member`, and the rest as for [`Solver::whole`]
    fn below(&self, class: DeclId, member: &Nominal, kept: TypeId) -> TypeId {
        let Some(binders) = self.binders(class) else {
            return self.whole(class, kept);
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
            return self.whole(class, kept);
        };
        let rigids = self.db.rigids(class);
        let carried: Vec<Option<TypeId>> = binders
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
                first.filter(|&first| carried.all(|other| other == Some(first)))
            })
            .collect();
        if !self.gradual {
            return self
                .approximate(class, &|slot| carried[slot])
                .unwrap_or(kept);
        }
        let args = (binders.iter().zip(carried))
            .map(|(binder, carried)| {
                Argument::Positional(carried.unwrap_or_else(|| self.db.unknown_of(binder.kind)))
            })
            .collect();
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args,
            kind: Kind::Type,
        })
    }

    /// `class` applied to `Unknown` for each binder. `Func` so applied is the
    /// gradual function type, as it's written.
    fn apply_unknown(&self, class: DeclId) -> TypeId {
        if self.intrinsic_decl(Intrinsic::Func) == Some(class) {
            return self.db.gradual_function();
        }
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
