//! Member schemas.
//!
//! `Fields[T]` is the schema of what a `fields` proxy of a `T` reads, and
//! `MutFields[T]` of what it sets: a keyed item for each public instance field
//! or property of `T`'s class, found in MRO order as member lookup finds it (see
//! [`super::member`]), so the first member of a name hides later ones even where
//! it isn't listed. A method isn't listed, nor a property's missing half.
//!
//! A read item is required, since every instance has the member. A set item is
//! optional, so a subclass's schema, with more items, is below its class's. A
//! class that isn't final may have a subclass with more members, so its read
//! schema admits any further key at `Value`, and its set schema any further key
//! with no value fitting it. A decorated member or a structural supertype, whose
//! members are unknown, opens the schema the same way.
//!
//! A union's read schema is the join of its members': a key all of them have is
//! required, at the union of its values, and one only some have is optional. Its
//! set schema is their meet: the keys all of them set, at the meet of their
//! values, admitting any other key with no value fitting it.

use std::ops::ControlFlow;

use super::{
    callable::drop_receiver,
    member::{FoundKind, Receiver, Signatures},
    *,
};
use crate::typeck::r#type::Scope;

/// The members a schema lists
struct Members {
    /// Each member's name, value, and whether every value of the target has it,
    /// in MRO order
    items: Vec<(SymbolId, TypeId, bool)>,
    /// Whether a value of the target may have members besides these
    open: bool,
}

impl Solver<'_> {
    /// The schema a closed `Fields` or `MutFields` of `target` stands for. A
    /// rigid or skolem target is every subtype of its bounds, whose members it
    /// can't list, so it relates only as itself.
    pub(super) fn fields(&self, target: TypeId, access: FieldAccess) -> Result<TypeId, Issue> {
        // A member's type may mention the schema it's listed in
        if self.fields_active.borrow().contains(&(target, access)) {
            return Err(Residual::Recursive.into());
        }
        self.fields_active.borrow_mut().push((target, access));
        let members = self.target_members(target, access);
        self.fields_active.borrow_mut().pop();
        let Some(members) = members? else {
            return Ok(self.db.unknown_schema());
        };
        let items = members
            .items
            .iter()
            .map(|&(name, value, required)| SchemaItem {
                multiplicity: match access {
                    FieldAccess::Read if required => Multiplicity::Required,
                    _ => Multiplicity::Optional,
                },
                element: Element::Keyed {
                    key: self.db.intern(Type::Literal(Literal::Sym(name))),
                    value,
                },
            });
        let rest = members.open.then(|| {
            self.further_keys(match access {
                FieldAccess::Read => self.db.top(),
                FieldAccess::Write => self.db.bottom(),
            })
        });
        Ok(self.db.intern(Type::Schema(items.chain(rest).collect())))
    }

    /// What every member schema is below, whatever its target: any number of
    /// symbol keys, at any value
    pub(super) fn fields_bound(&self) -> TypeId {
        self.db
            .intern(Type::Schema(vec![self.further_keys(self.db.top())].into()))
    }

    /// An item admitting any further symbol key at `value`
    fn further_keys(&self, value: TypeId) -> SchemaItem {
        SchemaItem {
            multiplicity: Multiplicity::Repeated,
            element: Element::Keyed {
                key: (self.db.intrinsic(Intrinsic::Sym)).unwrap_or(self.db.unknown()),
                value,
            },
        }
    }

    /// The members a closed target's schema lists, or `None` if they're dynamic
    fn target_members(
        &self,
        target: TypeId,
        access: FieldAccess,
    ) -> Result<Option<Members>, Issue> {
        let term = self.closed(target);
        let view = match self.head(term)? {
            Head::Infer(_) => return Err(Residual::Inference.into()),
            Head::Skolem(_) => return Err(Residual::Escape.into()),
            Head::Structural(view) => view,
            Head::Nominal(_) | Head::Meta(_) => return self.class_members(term, access),
        };
        match self.db.ty(view.ty) {
            Type::Rigid { .. } => Err(Residual::Escape.into()),
            Type::Union(members) if !members.is_empty() => {
                let mut combined: Option<Members> = None;
                for &member in members.iter() {
                    let UnionMember::Type(member) = member else {
                        return Err(Residual::Unsupported("the members of a projection").into());
                    };
                    let Some(next) = self.target_members(member, access)? else {
                        return Ok(None);
                    };
                    combined = Some(match combined {
                        Some(previous) => self.combine_members(previous, next, access)?,
                        None => next,
                    });
                }
                Ok(combined)
            }
            _ => self.class_members(term, access),
        }
    }

    /// The members two union members' schemas combine to list
    fn combine_members(
        &self,
        a: Members,
        b: Members,
        access: FieldAccess,
    ) -> Result<Members, Issue> {
        fn find(members: &Members, name: SymbolId) -> Option<&(SymbolId, TypeId, bool)> {
            (members.items.iter()).find(|&&(found, ..)| found == name)
        }
        let mut items = Vec::new();
        match access {
            FieldAccess::Read => {
                for &(name, value, required) in &a.items {
                    items.push(match find(&b, name) {
                        Some(&(_, other, also)) => (name, self.lub(value, other), required && also),
                        None => (name, value, false),
                    });
                }
                for &(name, value, _) in &b.items {
                    if find(&a, name).is_none() {
                        items.push((name, value, false));
                    }
                }
                Ok(Members {
                    items,
                    open: a.open || b.open,
                })
            }
            FieldAccess::Write => {
                for &(name, value, _) in &a.items {
                    if let Some(&(_, other, _)) = find(&b, name) {
                        items.push((name, self.meet(value, other)?, false));
                    }
                }
                let dropped = items.len() != a.items.len() || items.len() != b.items.len();
                Ok(Members {
                    items,
                    open: a.open || b.open || dropped,
                })
            }
        }
    }

    /// The members an instance's class lists, walking a literal or function to
    /// its intrinsic class
    fn class_members(&self, term: Term, access: FieldAccess) -> Result<Option<Members>, Issue> {
        let nominal = match self.receiver(term)? {
            Receiver::Instance(nominal) => nominal,
            Receiver::Object { .. } | Receiver::Generic(_) => {
                return Err(Residual::Unsupported("the members of a type object").into());
            }
            // Without std, no class has members
            Receiver::Missing => {
                return Ok(Some(Members {
                    items: Vec::new(),
                    open: true,
                }));
            }
            Receiver::Dynamic => return Ok(None),
        };
        let mut members = Members {
            items: Vec::new(),
            open: !self.db.declaration(nominal.declaration).source.is_final,
        };
        let mut seen = FxHashSet::default();
        let mut path = FxHashSet::default();
        self.mro(nominal, false, &mut path, 0, &mut |visited| {
            let Visited::Nominal(nominal) = visited else {
                members.open = true;
                return Ok(None::<()>);
            };
            for (key, member) in self.members(nominal) {
                // A member hides those of its name later in the MRO, even where
                // it isn't listed
                if key.special
                    || key.private
                    || member.scope() != Scope::Instance
                    || !seen.insert(key)
                    || !member.public()
                {
                    continue;
                }
                let value = match self.found(nominal, member).kind {
                    FoundKind::Field(ty) => Some(self.reify(ty)?),
                    FoundKind::Method(_) => None,
                    FoundKind::Property { getter, setter } => match access {
                        FieldAccess::Read => getter,
                        FieldAccess::Write => setter,
                    }
                    .map(|accessor| self.accessed(&accessor, access))
                    .transpose()?,
                    FoundKind::Unknown => {
                        members.open = true;
                        None
                    }
                };
                if let Some(value) = value {
                    members.items.push((key.name, value, true));
                }
            }
            Ok(None)
        })?;
        Ok(Some(members))
    }

    /// What a property's getter reads or its setter takes: the union over its
    /// overloads
    fn accessed(&self, accessor: &Signatures, access: FieldAccess) -> Result<TypeId, Issue> {
        let signatures = match accessor.overloads.is_empty() {
            true => accessor.implementation.as_slice(),
            false => &accessor.overloads,
        };
        let mut accessed = None;
        for &signature in signatures {
            let signature = self.reify(signature)?;
            // An accessor may be generic, as in its ambient channels, in what it
            // neither reads nor sets
            let (binders, body) = match self.db.ty(signature) {
                Type::Quantified { binders, body } => (Some(binders), *body),
                _ => (None, signature),
            };
            let unsupported = Residual::Unsupported("an accessor without a receiver");
            let (function, _) = drop_receiver(self.db, body).ok_or(unsupported)?;
            let Type::Function(function) = self.db.ty(function) else {
                unreachable!("a receiver is a function's parameter")
            };
            let value = match access {
                FieldAccess::Read => function.result,
                FieldAccess::Write => match self.db.ty(function.params) {
                    Type::Schema(items) => match items.first() {
                        Some(SchemaItem {
                            multiplicity: Multiplicity::Required,
                            element: Element::Positional(value),
                        }) => *value,
                        _ => return Err(Residual::Unsupported("a setter without a value").into()),
                    },
                    _ => return Err(Residual::Unsupported("a setter without a value").into()),
                },
            };
            let value = match binders {
                Some(binders) => {
                    let own = self.db.walk(value, |node, depth| match *self.db.ty(node) {
                        Type::Bound { reference, .. } if u32::from(reference.depth) == depth => {
                            ControlFlow::Break(())
                        }
                        _ => ControlFlow::Continue(()),
                    });
                    if own.is_break() {
                        let generic = "an accessor generic in what it reads or sets";
                        return Err(Residual::Unsupported(generic).into());
                    }
                    let unknowns: Vec<_> = (binders.iter())
                        .map(|binder| self.db.unknown_of(binder.kind))
                        .collect();
                    self.db.instantiate(value, &unknowns)
                }
                None => value,
            };
            accessed = Some(match accessed {
                Some(previous) => self.lub(previous, value),
                None => value,
            });
        }
        accessed.ok_or_else(|| Residual::Unsupported("an accessor without a signature").into())
    }
}
