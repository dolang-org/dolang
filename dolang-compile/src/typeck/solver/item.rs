//! Item projections: `IndexItem[S, K]` and `AssignItem[S, K]`.
//!
//! A key selects items of a schema's keyed view (see [`Database::promoted`]),
//! where each position is keyed by its index. Each member of the key goes to the
//! literal item it is, if there is one. Otherwise it goes to the literal items
//! lying inside it and to the domains that own it, as an actual keyed item goes
//! to an expected schema's domains (see [`Solver::owning`]). `IndexItem` joins
//! the selected values, what a read may give, and `AssignItem` meets them, what
//! a write must fit. A key member the schema doesn't admit whole is unadmitted.
//!
//! A rigid key is known only by its bound, so it selects exactly only where
//! every item its bound selects has the same value.
//!
//! The meet needs no intersection types where it matters: of two values, one
//! below the other gives the lower, and two literals or classes that can't share
//! a value give the bottom type. Any other meet is the dynamic type, leaving the
//! write unchecked.

use super::*;

impl Solver<'_> {
    /// A closed union with its item projections evaluated, or `None` if it has
    /// none
    pub(super) fn evaluate_items(&self, ty: TypeId) -> Result<Option<TypeId>, Issue> {
        let Type::Union(members) = self.db.ty(ty) else {
            return Ok(None);
        };
        if !members.iter().any(|member| member.key().is_some()) {
            return Ok(None);
        }
        let members = (members.iter())
            .map(|&member| match member {
                UnionMember::IndexItem(schema, key) => {
                    Ok(UnionMember::Type(self.item(schema, key, false)?))
                }
                UnionMember::AssignItem(schema, key) => {
                    Ok(UnionMember::Type(self.item(schema, key, true)?))
                }
                _ => Ok(member),
            })
            .collect::<Result<Vec<_>, Issue>>()?;
        Ok(Some(self.db.intern(Type::Union(members.into()))))
    }

    /// The join, or with `meet` the meet, of the values of a schema's items a key
    /// selects
    fn item(&self, schema: TypeId, key: TypeId, meet: bool) -> Result<TypeId, Issue> {
        if let Type::Unknown(_) = self.db.ty(schema) {
            return Ok(self.db.unknown());
        }
        let promoted = match self.db.promoted(schema) {
            Promotion::Promoted(promoted) => promoted,
            Promotion::Pending => {
                return Err(Residual::Unsupported("a schema whose keyed view is pending").into());
            }
            Promotion::Conflict => return Err(Issue::Contradiction(Contradiction::Conflict)),
        };
        // An included schema not yet known may hold any item
        if let Some(&opaque) = promoted.opaque.first() {
            return match self.db.ty(opaque) {
                Type::Unknown(_) => Ok(self.db.unknown()),
                _ => Err(Residual::Unsupported("a schema including a schema not yet known").into()),
            };
        }
        let mut literals: Vec<(TypeId, TypeId)> = (promoted.fixed.iter().enumerate())
            .map(|(i, &value)| {
                (
                    self.db.intern(Type::Literal(Literal::Int(i as i128))),
                    value,
                )
            })
            .collect();
        let mut domains: Vec<(TypeId, TypeId)> = Vec::new();
        if !promoted.varying.is_empty() {
            let int = self
                .db
                .intrinsic(Intrinsic::Int)
                .ok_or(Residual::MissingIntrinsic(Intrinsic::Int))?;
            domains.extend(promoted.varying.iter().map(|&value| (int, value)));
        }
        for &(_, key, value) in &promoted.keyed {
            match self.db.literal(key) {
                Some(_) => literals.push((self.db.regular(key), value)),
                None => domains.push((key, value)),
            }
        }
        let keys: Vec<TypeId> = domains.iter().map(|&(key, _)| key).collect();
        let mut values = Vec::new();
        for member in self.union_members(key) {
            let UnionMember::Type(member) = member else {
                return Err(Residual::Unsupported("a key with projections").into());
            };
            // A rigid key is known only by its bound. Where each item the bound
            // selects has the same value, the rigid selects that value too.
            if self.rigid(member)?.is_some() {
                let Some(bound) = self.rigid_bound(member) else {
                    return Err(Residual::Unsupported("a rigid key without a bound").into());
                };
                let joined = self.item(schema, bound, false)?;
                if joined != self.item(schema, bound, true)? {
                    return Err(
                        Residual::Unsupported("a rigid key selecting different values").into(),
                    );
                }
                values.push(joined);
                continue;
            }
            let member = self.db.regular(member);
            // A literal owns its own item
            if let Some(&(_, value)) = literals.iter().find(|&&(key, _)| key == member) {
                values.push(value);
                continue;
            }
            if self.db.literal(member).is_none() {
                for &(key, value) in &literals {
                    match self.probe(key, member)? {
                        Status::Proven => values.push(value),
                        Status::Contradicted => {}
                        Status::Unresolved => {
                            return Err(Residual::Unsupported(
                                "a literal item a key may or may not select",
                            )
                            .into());
                        }
                    }
                }
            }
            let mut whole = false;
            for (d, admits) in self.owning(member, &keys)? {
                whole |= admits;
                values.push(domains[d].1);
            }
            if !whole {
                let view = self.closed(member);
                if self.db.literal(member).is_some() || matches!(self.head(view)?, Head::Nominal(_))
                {
                    return Err(Issue::Contradiction(Contradiction::Unadmitted(member)));
                }
                return Err(Residual::Unsupported("a key the schema may or may not admit").into());
            }
        }
        if !meet {
            let members = values.into_iter().map(UnionMember::Type).collect();
            return Ok(self.db.intern(Type::Union(members)));
        }
        let mut met = self.db.top();
        for value in values {
            met = self.meet(met, value)?;
        }
        Ok(met)
    }

    /// The meet of two closed types, where it needs no intersection: the lower
    /// of two that are ordered, the bottom type for two literals or classes that
    /// can't share a value, and otherwise the dynamic type
    fn meet(&self, a: TypeId, b: TypeId) -> Result<TypeId, Issue> {
        if a == b {
            return Ok(a);
        }
        let below = self.probe(a, b)?;
        if below == Status::Proven {
            return Ok(a);
        }
        let above = self.probe(b, a)?;
        if above == Status::Proven {
            return Ok(b);
        }
        let disjoint = below == Status::Contradicted
            && above == Status::Contradicted
            && self.class_like(a)?
            && self.class_like(b)?;
        Ok(match disjoint {
            true => self.db.bottom(),
            false => self.db.unknown(),
        })
    }

    /// Whether a type is a literal or an instance of a class. The runtime gives a
    /// class one supertype, so two classes neither below the other share no
    /// value.
    fn class_like(&self, ty: TypeId) -> Result<bool, Issue> {
        if self.db.literal(ty).is_some() {
            return Ok(true);
        }
        Ok(match self.head(self.closed(ty))? {
            Head::Nominal(nominal) => {
                self.db.declaration(nominal.declaration).source.kind == DeclKind::Class
            }
            _ => false,
        })
    }
}
