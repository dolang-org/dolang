//! Item projections: `IndexItem[S, K]`, `AssignItem[S, K]` and
//! `InsertItem[S, K]`, and the restrictions `Deletable[S, K]` and
//! `Clearable[S]`.
//!
//! A key selects items of a schema's keyed view (see [`Database::promoted`]),
//! where each position is keyed by its index. Each member of the key goes to the
//! literal item it is, if there is one. Otherwise it goes to the literal items
//! lying inside it and to the domains that own it, as an actual keyed item goes
//! to an expected schema's domains (see [`Solver::owning`]). An `Int` key
//! selects every fixed position, as it selects every varying one: an index out
//! of range is a runtime error, as an array's is. `IndexItem` joins the selected
//! values, what a read may give, and `AssignItem` meets them, what a write must
//! fit. A key member the schema doesn't admit whole is unadmitted, as a literal
//! index past the fixed positions is.
//!
//! `InsertItem` meets the values as `AssignItem` does, where each item its key
//! selects is keyed and repeated, as one more of it must be. `Deletable` is its
//! schema where each item its key selects is keyed and admits none, and
//! `Clearable` where every item admits none. A position is never inserted or
//! deleted, since that would move the positions after it.
//!
//! A rigid or skolem key is known only by its bounds, including those its
//! projections imply (see [`Database::implied_bounds`]), so it selects a value
//! only where every item one of them selects has that value. Otherwise an
//! `IndexItem` by it stays unevaluated, and is related by widening its key
//! toward the bound. Its restriction holds where it holds for a bound.
//!
//! A write relates to each value its key selects, so it needs no meet of them.
//! Elsewhere the meet is taken without intersection types: of two values, one
//! below the other gives the lower, and two that can't share a value give the
//! bottom type. Any other meet is the dynamic type, or unresolved for a strict
//! unit's solver.

use super::*;
use crate::typeck::r#type::Promoted;

/// How an item a key selects occurs in its schema
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Occurrence {
    /// A position, keyed by its index
    Position,
    /// A keyed item
    Keyed(Multiplicity),
    /// An item of the dynamic schema, or of an included one
    Unknown,
}

/// An item a key selects
#[derive(Clone, Copy, Debug)]
enum Selected {
    /// An item, with the key selecting it: its literal key if it has one, else
    /// the key's member
    Item {
        value: TypeId,
        occurrence: Occurrence,
        key: TypeId,
    },
    /// What a rigid member of the key selects, as its bounds do
    Rigid(TypeId),
}

/// A change to the items a key selects
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// Adding one more of each, as `InsertItem` does
    Insert,
    /// Removing each, as `Deletable` requires
    Delete,
}

impl Change {
    /// Whether an item occurring so admits the change
    fn admits(self, occurrence: Occurrence) -> bool {
        match (self, occurrence) {
            (_, Occurrence::Unknown) => true,
            (_, Occurrence::Position) => false,
            (Self::Insert, Occurrence::Keyed(multiplicity)) => {
                multiplicity == Multiplicity::Repeated
            }
            (Self::Delete, Occurrence::Keyed(multiplicity)) => {
                multiplicity != Multiplicity::Required
            }
        }
    }

    /// The contradiction of a key selecting an item that doesn't admit it
    fn refused(self, key: TypeId) -> Contradiction {
        match self {
            Self::Insert => Contradiction::Unrepeatable(key),
            Self::Delete => Contradiction::Required(key),
        }
    }
}

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
                UnionMember::InsertItem(schema, key) => {
                    self.changed(schema, key, Change::Insert)?;
                    Ok(UnionMember::Type(self.item(schema, key, true)?))
                }
                _ => Ok(member),
            })
            .collect::<Result<Vec<_>, Issue>>()?;
        Ok(Some(self.db.intern(Type::Union(members.into()))))
    }

    /// A union holding a skolem with its item projections evaluated, or `None`
    /// if it can't be. A skolem key is known only by its bounds, so it selects
    /// a value where every item one of its bounds selects has that value. What
    /// remains must be closed.
    pub(super) fn evaluate_skolem_items(&self, view: TypeView) -> Result<Option<TypeId>, Issue> {
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(None);
        };
        let mut replaced = Vec::with_capacity(members.len());
        for &member in members.iter() {
            let (Some(key), Some(schema)) = (member.key(), member.projected()) else {
                replaced.push(member);
                continue;
            };
            let Term::Skolem(id) = self.resolve(view.child(key))? else {
                replaced.push(member);
                continue;
            };
            let Ok(schema) = self.reify(view.child(schema)) else {
                return Ok(None);
            };
            let Ok(value) = self.bounded_item(schema, Abstract::Skolem(id)) else {
                return Ok(None);
            };
            if let UnionMember::InsertItem(..) = member {
                match self.bounded_change(schema, Abstract::Skolem(id), Change::Insert) {
                    Ok(()) => {}
                    Err(Issue::Residual(_)) => return Ok(None),
                    Err(issue) => return Err(issue),
                }
            }
            // A closed value means the same in the view's environment
            replaced.push(UnionMember::Type(value));
        }
        let replaced = self.db.intern(Type::Union(replaced.into()));
        Ok(self.reify(view.child(replaced)).ok())
    }

    /// A closed restriction's schema, if the restriction holds
    pub(super) fn restricted(
        &self,
        schema: TypeId,
        restriction: Restriction,
    ) -> Result<TypeId, Issue> {
        match restriction {
            Restriction::Deletable(key) => self.changed(schema, key, Change::Delete)?,
            Restriction::Clearable => self.clearable(schema)?,
        }
        Ok(schema)
    }

    /// A restriction by a skolem key with its schema, if it holds, or `None` if
    /// it can't be evaluated. A skolem key is known only by its bounds, so the
    /// restriction holds where it holds for one of them.
    pub(super) fn evaluate_skolem_restriction(
        &self,
        view: TypeView,
    ) -> Result<Option<TypeId>, Issue> {
        let Type::Restricted {
            schema,
            restriction: Restriction::Deletable(key),
        } = *self.db.ty(view.ty)
        else {
            return Ok(None);
        };
        let Term::Skolem(id) = self.resolve(view.child(key))? else {
            return Ok(None);
        };
        let Ok(schema) = self.reify(view.child(schema)) else {
            return Ok(None);
        };
        match self.bounded_change(schema, Abstract::Skolem(id), Change::Delete) {
            Ok(()) => Ok(Some(schema)),
            Err(Issue::Residual(_)) => Ok(None),
            Err(issue) => Err(issue),
        }
    }

    /// Check that each item of a schema a key selects admits a change
    fn changed(&self, schema: TypeId, key: TypeId, change: Change) -> Result<(), Issue> {
        for selected in self.selected_items(schema, key)? {
            match selected {
                Selected::Item {
                    occurrence, key, ..
                } => {
                    if !change.admits(occurrence) {
                        return Err(Issue::Contradiction(change.refused(key)));
                    }
                }
                Selected::Rigid(member) => {
                    self.bounded_change(schema, Abstract::Rigid(member), change)?
                }
            }
        }
        Ok(())
    }

    /// Check that each item of a schema a rigid or skolem key selects admits a
    /// change: those one of its bounds selects all do. Otherwise, why the first
    /// bound's don't.
    fn bounded_change(&self, schema: TypeId, key: Abstract, change: Change) -> Result<(), Issue> {
        let mut first = None;
        for (bound, _) in self.abstract_bounds(key) {
            let checked = (self.reify(bound).map_err(Issue::from))
                .and_then(|bound| self.changed(schema, bound, change));
            match checked {
                Ok(()) => return Ok(()),
                Err(issue) => {
                    first.get_or_insert(issue);
                }
            }
        }
        Err(first.unwrap_or_else(|| Residual::Unsupported("a key without a bound").into()))
    }

    /// Check that every item of a schema admits none, through its inclusions.
    /// A position's key is its index. An included schema that isn't known may
    /// hold anything.
    fn clearable(&self, schema: TypeId) -> Result<(), Issue> {
        let Some(promoted) = self.keyed_view(schema)? else {
            return Ok(());
        };
        if !promoted.fixed.is_empty() {
            let first = self.db.intern(Type::Literal(Literal::Int(0)));
            return Err(Issue::Contradiction(Contradiction::Required(first)));
        }
        let required = (promoted.keyed.iter())
            .find(|&&(multiplicity, _, _)| multiplicity == Multiplicity::Required);
        if let Some(&(_, key, _)) = required {
            let key = self.db.regular(key);
            return Err(Issue::Contradiction(Contradiction::Required(key)));
        }
        if (promoted.opaque.iter()).any(|&opaque| !matches!(self.db.ty(opaque), Type::Unknown(_))) {
            return Err(Residual::Unsupported("a schema including a schema not yet known").into());
        }
        Ok(())
    }

    /// The value a rigid or skolem key selects from a schema: that of every item
    /// one of its bounds selects, where they all have it. Otherwise, why the
    /// first bound selects none.
    fn bounded_item(&self, schema: TypeId, key: Abstract) -> Result<TypeId, Issue> {
        let mut first = None;
        for (bound, _) in self.abstract_bounds(key) {
            match self.selected_by_bound(schema, bound) {
                Ok(value) => return Ok(value),
                Err(issue) => {
                    first.get_or_insert(issue);
                }
            }
        }
        Err(first.unwrap_or_else(|| Residual::Unsupported("a key without a bound").into()))
    }

    /// The value every item of a schema a bound selects has
    fn selected_by_bound(&self, schema: TypeId, bound: Term) -> Result<TypeId, Issue> {
        let bound = self.reify(bound)?;
        let joined = self.item(schema, bound, false)?;
        if joined != self.item(schema, bound, true)? {
            return Err(Residual::Unsupported("a bounded key selecting different values").into());
        }
        Ok(joined)
    }

    /// The join, or with `meet` the meet, of the values of a schema's items a key
    /// selects
    pub(crate) fn schema_item(&self, schema: TypeId, key: TypeId) -> Result<TypeId, Issue> {
        self.item(schema, key, true)
    }

    fn item(&self, schema: TypeId, key: TypeId, meet: bool) -> Result<TypeId, Issue> {
        let values = self.selected(schema, key)?;
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

    /// The values of a closed `AssignItem` a term is, where its key selects
    /// more than one. A write must fit each, which needs no meet of them.
    pub(super) fn assigned(&self, term: Term) -> Result<Option<Vec<TypeId>>, Issue> {
        let Term::View(view) = self.resolve(term)? else {
            return Ok(None);
        };
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(None);
        };
        let &[
            member @ (UnionMember::AssignItem(schema, key) | UnionMember::InsertItem(schema, key)),
        ] = &members[..]
        else {
            return Ok(None);
        };
        let (Ok(schema), Ok(key)) = (self.reify(view.child(schema)), self.reify(view.child(key)))
        else {
            return Ok(None);
        };
        // An insertion its schema doesn't admit relates as its evaluation does
        if let UnionMember::InsertItem(..) = member
            && self.changed(schema, key, Change::Insert).is_err()
        {
            return Ok(None);
        }
        // Anything else relates as the meet does
        let Ok(selected) = self.selected(schema, key) else {
            return Ok(None);
        };
        let mut values = Vec::new();
        for value in selected {
            if !values.contains(&value) {
                values.push(value);
            }
        }
        Ok((values.len() > 1).then_some(values))
    }

    /// The values of a schema's items a key selects
    fn selected(&self, schema: TypeId, key: TypeId) -> Result<Vec<TypeId>, Issue> {
        (self.selected_items(schema, key)?.into_iter())
            .map(|selected| match selected {
                Selected::Item { value, .. } => Ok(value),
                // A rigid key is known only by its bounds. Where each item one
                // of them selects has the same value, the rigid selects that
                // value too.
                Selected::Rigid(member) => self.bounded_item(schema, Abstract::Rigid(member)),
            })
            .collect()
    }

    /// A closed schema's keyed view, through its transparent declarations, or
    /// `None` for the dynamic schema
    fn keyed_view(&self, schema: TypeId) -> Result<Option<Promoted>, Issue> {
        let schema = self.exposed_schema(schema)?.unwrap_or(schema);
        if let Type::Unknown(_) = self.db.ty(schema) {
            return Ok(None);
        }
        match self.db.promoted(schema) {
            Promotion::Promoted(promoted) => Ok(Some(promoted)),
            Promotion::Pending => {
                Err(Residual::Unsupported("a schema whose keyed view is pending").into())
            }
            Promotion::Conflict => Err(Issue::Contradiction(Contradiction::Conflict)),
        }
    }

    /// The items of a schema a key selects
    fn selected_items(&self, schema: TypeId, key: TypeId) -> Result<Vec<Selected>, Issue> {
        let unknown = |key| Selected::Item {
            value: self.db.unknown(),
            occurrence: Occurrence::Unknown,
            key,
        };
        let Some(promoted) = self.keyed_view(schema)? else {
            return Ok(vec![unknown(key)]);
        };
        // An included schema not yet known may hold any item
        if let Some(&opaque) = promoted.opaque.first() {
            return match self.db.ty(opaque) {
                Type::Unknown(_) => Ok(vec![unknown(key)]),
                _ => Err(Residual::Unsupported("a schema including a schema not yet known").into()),
            };
        }
        let mut literals: Vec<(TypeId, TypeId, Occurrence)> = (promoted.fixed.iter().enumerate())
            .map(|(i, &value)| {
                (
                    self.db.intern(Type::Literal(Literal::Int(i as i128))),
                    value,
                    Occurrence::Position,
                )
            })
            .collect();
        let mut domains: Vec<(TypeId, TypeId, Occurrence)> = Vec::new();
        if !promoted.varying.is_empty() {
            let int = self
                .db
                .intrinsic(Intrinsic::Int)
                .ok_or(Residual::MissingIntrinsic(Intrinsic::Int))?;
            domains
                .extend((promoted.varying.iter()).map(|&value| (int, value, Occurrence::Position)));
        }
        for &(multiplicity, key, value) in &promoted.keyed {
            let occurrence = Occurrence::Keyed(multiplicity);
            match self.db.literal(key) {
                Some(_) => literals.push((self.db.regular(key), value, occurrence)),
                None => domains.push((key, value, occurrence)),
            }
        }
        let keys: Vec<TypeId> = domains.iter().map(|&(key, _, _)| key).collect();
        let mut selected = Vec::new();
        for member in self.union_members(key) {
            let UnionMember::Type(member) = member else {
                return Err(Residual::Unsupported("a key with projections").into());
            };
            if self.rigid(member)?.is_some() {
                selected.push(Selected::Rigid(member));
                continue;
            }
            let member = self.db.regular(member);
            // A literal owns its own item
            if let Some(&(key, value, occurrence)) =
                literals.iter().find(|&&(key, _, _)| key == member)
            {
                selected.push(Selected::Item {
                    value,
                    occurrence,
                    key,
                });
                continue;
            }
            if self.db.literal(member).is_none() {
                for &(key, value, occurrence) in &literals {
                    match self.probe(key, member)? {
                        Status::Proven => selected.push(Selected::Item {
                            value,
                            occurrence,
                            key,
                        }),
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
            // An index of no particular position may be any of them
            let mut whole = !promoted.fixed.is_empty()
                && self
                    .db
                    .intrinsic(Intrinsic::Int)
                    .is_some_and(|int| int == member);
            for (d, admits) in self.owning(member, &keys)? {
                whole |= admits;
                let (_, value, occurrence) = domains[d];
                selected.push(Selected::Item {
                    value,
                    occurrence,
                    key: member,
                });
            }
            if !whole {
                let view = self.closed(member);
                if self.db.literal(member).is_some()
                    || matches!(self.head(view)?, Head::Nominal(_) | Head::Meta(_))
                {
                    return Err(Issue::Contradiction(Contradiction::Unadmitted(member)));
                }
                return Err(Residual::Unsupported("a key the schema may or may not admit").into());
            }
        }
        Ok(selected)
    }

    /// The meet of two closed types, where it needs no intersection: the lower
    /// of two that are ordered, and otherwise the dynamic type, or unresolved for
    /// a strict unit's solver. Two classes neither below the other share the
    /// values of a subclass of both, so they meet in the bottom type only when
    /// one is a literal, or final and doesn't reach the other, or when their
    /// nearest disjoint ancestors are unrelated.
    pub(super) fn meet(&self, a: TypeId, b: TypeId) -> Result<TypeId, Issue> {
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
            && (self.class_like(a)?
                && self.class_like(b)?
                && (self.db.literal(a).is_some()
                    || self.db.literal(b).is_some()
                    || self.final_outside(a, b)
                    || self.final_outside(b, a))
                || self.disjoint_apart(a, b));
        match (disjoint, self.gradual) {
            (true, _) => Ok(self.db.bottom()),
            (false, true) => Ok(self.db.unknown()),
            (false, false) => {
                Err(Residual::Unsupported("a meet of types neither ordered nor disjoint").into())
            }
        }
    }

    /// Whether a type is a literal or an instance of a class. A class has values
    /// outside every class it doesn't reach, its own instances or those of a
    /// subclass inheriting only it, so it isn't below one. That doesn't make the
    /// two disjoint: a subclass of both is in each.
    pub(super) fn class_like(&self, ty: TypeId) -> Result<bool, Issue> {
        if self.db.literal(ty).is_some() {
            return Ok(true);
        }
        // A metatype's values are `Type`'s
        Ok(match self.widened_head(self.closed(ty))? {
            Head::Nominal(nominal) => {
                self.db.declaration(nominal.declaration).source.kind == DeclKind::Class
            }
            _ => false,
        })
    }

    /// Whether two types' classes have unrelated nearest disjoint ancestors.
    /// Unknown when either has none.
    fn disjoint_apart(&self, a: TypeId, b: TypeId) -> bool {
        let (Ok(Some(a)), Ok(Some(b))) = (self.start(a), self.start(b)) else {
            return false;
        };
        self.db.disjoint_apart(a.declaration, b.declaration)
    }

    /// Whether `actual` is an instance of a final class that doesn't reach
    /// `expected`'s class. A final class has no subclasses but those declared
    /// with it, so none of its values is also an instance of another class,
    /// however that class is applied. Unknown when either has no class, or the
    /// ancestor search can't finish.
    pub(super) fn final_outside(&self, actual: TypeId, expected: TypeId) -> bool {
        let (Ok(Some(actual)), Ok(Some(expected))) = (self.start(actual), self.start(expected))
        else {
            return false;
        };
        self.db.declaration(actual.declaration).source.is_final
            && self.reaches(&actual, expected.declaration) == Some(false)
    }
}
