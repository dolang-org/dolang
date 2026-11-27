//! Partial evaluation of projections: the unions with members projected from a
//! schema (`IndexItem`, `Keys`, `Values`, `Entries`, `Expand` and the write
//! projections), restrictions, and member schemas.
//!
//! [`Solver::evaluate`] is the one place they're evaluated. Its result is
//! one of three:
//!
//! - **exact**: what the projection is, one step further. That's its
//!   reification once its environment is substituted, a skolem key's selection
//!   by its bounds (see [`Solver::evaluate_skolem_items`]), or what its
//!   schema's shape determines before its items' types are known (see
//!   [`Solver::by_shape`]). A closed projection is evaluated once its schema is
//!   exposed.
//! - **upper**: a term above it, which only the actual side of a judgment may
//!   take, with the step that widens it. A rigid schema widens to its bound,
//!   and a rigid or skolem key toward its bound (see
//!   [`Solver::widened_projection`]). A variable key never widens, as its
//!   solution may select a precise item.
//! - **lower**: a term below it, which only the expected side of a judgment
//!   may take. A write through a rigid's member schema, or a key of one,
//!   lowers to its bound's, which every instance shares (see
//!   [`Solver::lowered_projection`]); a judgment against it is derived. The keys of a
//!   rigid schema lower to those its bound requires, which are only some of
//!   the keys it admits: that's sufficient, so a judgment against them is
//!   only probed. `AssignItem` is antitone in its key, so its key never
//!   approximates, on either side.
//! - **stuck**: left for the relational rules (see the `projections` rule),
//!   noting whether it waits on inference.
//!
//! A write whose key selects several values is the meet of them, which the
//! expected side relates to each instead of computing it (see
//! [`Solver::write_selection`]). It's taken before the expected side's head, which
//! would compute the meet.
//!
//! Each caller states which approximation it takes. A variable's
//! bound is evaluated exactly when it's recorded, and a lower bound that would
//! hold a skolem the variable can't see takes its upper approximation instead
//! (see [`Solver::add_bound`]).

use super::item::Change;
use super::*;

/// Whether a caller of [`Solver::evaluate`] takes a term above the projection,
/// as only the actual side of a judgment may, or below it, as only the
/// expected side may
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Approximation {
    Exact,
    Upper,
    Lower,
}

/// What [`Solver::evaluate`] found a projection to be
#[derive(Clone, Debug)]
pub(super) enum Evaluation {
    /// What it is, evaluated one step further
    Exact(Term),
    /// A term above it, with the step that widens it
    Upper(Term, Step),
    /// A term below it that every instance of its rigids shares, with the
    /// step that reaches it
    Lower(Term, Step),
    /// A term below it that's only sufficient: what's below the term fits
    /// the projection, but what isn't may fit it too
    Sufficient(Term),
    /// Neither, leaving it to the relational rules: `waiting` if that's
    /// because it waits on inference
    Stuck { waiting: bool },
}

impl Solver<'_> {
    /// Whether a type is one [`Self::evaluate`] evaluates: a union with a
    /// projection, a restriction, a member schema or a repeated schema
    pub(super) fn projective(&self, ty: TypeId) -> bool {
        match self.db.ty(ty) {
            Type::Union(members) => members.iter().any(|member| member.projected().is_some()),
            Type::Restricted { .. } | Type::Fields { .. } | Type::Repeat { .. } => true,
            _ => false,
        }
    }

    /// Evaluate a projection in `view` one step, or with
    /// [`Approximation::Upper`], a lone projection that is stuck to what it
    /// widens to, or with [`Approximation::Lower`], a union of projections that
    /// is stuck to what it lowers to (see the module documentation)
    pub(super) fn evaluate(
        &self,
        view: TypeView,
        approximation: Approximation,
    ) -> Result<Evaluation, Issue> {
        let exact = self.exact(view)?;
        let Evaluation::Stuck { .. } = exact else {
            return Ok(exact);
        };
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(exact);
        };
        match approximation {
            Approximation::Exact => {}
            Approximation::Upper => {
                if let &[member] = &members[..]
                    && let Some((widened, step)) = self.widened_projection(view, member)?
                {
                    return Ok(Evaluation::Upper(widened, step));
                }
            }
            Approximation::Lower => {
                if let Some(lowered) = self.lowered_projection(view, members)? {
                    return Ok(lowered);
                }
            }
        }
        Ok(exact)
    }

    /// A projection member of the union `view` evaluated alone, as
    /// [`Self::evaluate`] does
    pub(super) fn evaluate_member(
        &self,
        view: TypeView,
        member: UnionMember,
        approximation: Approximation,
    ) -> Result<Evaluation, Issue> {
        let lone = self.db.intern(Type::Union(vec![member].into()));
        let lone = TypeView {
            ty: lone,
            environment: view.environment,
        };
        self.evaluate(lone, approximation)
    }

    /// The `index`th member of the union `view`, a projection, as the actual
    /// side of a judgment takes it: what it evaluates to, or else what it
    /// widens to, with the step that reaches it. `None` if it's stuck.
    pub(super) fn actual_member(
        &self,
        view: TypeView,
        member: UnionMember,
        index: usize,
    ) -> Result<Option<(Term, Step)>, Issue> {
        Ok(
            match self.evaluate_member(view, member, Approximation::Upper)? {
                Evaluation::Exact(term) => Some((term, Step::UnionMember(index))),
                Evaluation::Upper(term, step) => Some((term, step)),
                Evaluation::Lower(..) | Evaluation::Sufficient(_) | Evaluation::Stuck { .. } => {
                    None
                }
            },
        )
    }

    /// The exact step of [`Self::evaluate`]
    fn exact(&self, view: TypeView) -> Result<Evaluation, Issue> {
        let open = view.environment != self.empty_environment();
        let exact = |ty| Ok(Evaluation::Exact(self.closed(ty)));
        let stuck = |waiting| Ok(Evaluation::Stuck { waiting });
        match *self.db.ty(view.ty) {
            // A projection is evaluated once its schema is substituted
            Type::Union(_) if open => match self.reify(Term::View(view)) {
                Ok(reified) => exact(reified),
                // A skolem's projection is evaluated only as an item projection
                // by a skolem key, or by its schema's shape; otherwise it
                // relates only as itself
                Err(Residual::Escape) => {
                    if let Some(evaluated) = self.evaluate_skolem_items(view)? {
                        return exact(evaluated);
                    }
                    match self.by_shape(view)? {
                        Some(evaluated) => Ok(Evaluation::Exact(evaluated)),
                        None => stuck(false),
                    }
                }
                // `Keys`, and an item of a homogeneous schema, may be known by
                // its schema's shape before its items' types are
                Err(Residual::Inference) => match self.by_shape(view)? {
                    Some(evaluated) => Ok(Evaluation::Exact(evaluated)),
                    None => stuck(true),
                },
                Err(residual) => Err(residual.into()),
            },
            Type::Union(ref members) => {
                // Interning evaluates a projection only of a schema written
                // out, so one of a transparent declaration is evaluated once
                // exposed
                if let Some(exposed) = self.exposed_projections(members)? {
                    return exact(exposed);
                }
                // A rigid schema's keys are known by its bound's shape
                if let Some(keys) = self.keys_by_shape(view)? {
                    return exact(keys);
                }
                // An item projection takes the solver to select by its key.
                // One that can't be evaluated is left for the rules of unions.
                match self.evaluate_items(view.ty) {
                    Ok(Some(evaluated)) => exact(evaluated),
                    Ok(None) | Err(Issue::Residual(_)) => stuck(false),
                    Err(issue) => Err(issue),
                }
            }
            // A restriction is evaluated once its schema and key are
            // substituted
            Type::Restricted { .. } if open => match self.reify(Term::View(view)) {
                Ok(reified) => exact(reified),
                // A skolem key is known only by its bounds
                Err(Residual::Escape) => match self.evaluate_skolem_restriction(view)? {
                    Some(schema) => exact(schema),
                    None => stuck(false),
                },
                Err(residual) => Err(residual.into()),
            },
            // A restriction that holds is its schema. One that can't be
            // evaluated relates only as itself.
            Type::Restricted {
                schema,
                restriction,
            } => match self.restricted(schema, restriction) {
                Ok(schema) => exact(schema),
                Err(Issue::Residual(_)) => stuck(false),
                Err(issue) => Err(issue),
            },
            // A member schema is evaluated once its target is substituted. A
            // rigid or skolem target's relates only as itself.
            Type::Fields { .. } if open => match self.reify(Term::View(view)) {
                Ok(reified) => exact(reified),
                Err(Residual::Escape) => stuck(false),
                Err(residual) => Err(residual.into()),
            },
            Type::Fields { target, access } => match self.fields(target, access) {
                Ok(schema) => exact(schema),
                Err(Issue::Residual(_)) => stuck(false),
                Err(issue) => Err(issue),
            },
            // A repeated schema is reduced once its count is substituted (see
            // `Database::reduce_repeat`). One whose count is rigid or a skolem
            // relates only as itself, or by what it exposes.
            Type::Repeat { .. } if open => match self.reify(Term::View(view)) {
                Ok(reified) => exact(reified),
                Err(Residual::Escape) => stuck(false),
                Err(Residual::Inference) => stuck(true),
                Err(residual) => Err(residual.into()),
            },
            _ => stuck(false),
        }
    }

    /// A lone `Keys` projection in `view` evaluated by the shape of its schema,
    /// whose items' types may be unsolved: `Int` for a schema of positions only,
    /// or the closed keys of one with keyed items only. `None` if it has both,
    /// as a key may then be an index, or the shape isn't known. A rigid schema
    /// has its bound's shape, so it's `Int` if its bound is of positions only;
    /// a rigid narrower than a keyed bound may have fewer keys.
    fn keys_by_shape(&self, view: TypeView) -> Result<Option<TypeId>, Residual> {
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(None);
        };
        let &[UnionMember::Keys(schema)] = &members[..] else {
            return Ok(None);
        };
        let mut positions = false;
        let mut rigid = false;
        let mut keys = Vec::new();
        let mut pending = vec![view.child(schema)];
        while let Some(schema) = pending.pop() {
            self.spend()?;
            let Term::View(schema) = self.resolve(schema)? else {
                return Ok(None);
            };
            if self.rigid(schema.ty)?.is_some() {
                let Some(&(bound, _)) = self.rigid_bounds(schema.ty).first() else {
                    return Ok(None);
                };
                rigid = true;
                pending.push(self.closed(bound));
                continue;
            }
            let Type::Schema(items) = self.db.ty(schema.ty) else {
                return Ok(None);
            };
            for item in items.iter() {
                match item.element {
                    Element::Positional(_) => positions = true,
                    Element::Keyed { key, .. } => match self.reify(schema.child(key)) {
                        Ok(key) => keys.push(UnionMember::Type(key)),
                        Err(_) => return Ok(None),
                    },
                    Element::Include(inner) => pending.push(schema.child(inner)),
                }
            }
        }
        Ok(match (positions, keys.is_empty()) {
            (true, true) => self.db.intrinsic(Intrinsic::Int),
            (false, _) if !rigid => Some(self.db.intern(Type::Union(keys.into()))),
            _ => None,
        })
    }

    /// A lone projection in `view` evaluated by the shape of its schema alone,
    /// as [`Self::keys_by_shape`] and [`Self::item_by_shape`] do
    fn by_shape(&self, view: TypeView) -> Result<Option<Term>, Residual> {
        if let Some(keys) = self.keys_by_shape(view)? {
            return Ok(Some(self.closed(keys)));
        }
        self.item_by_shape(view)
    }

    /// Whether a key is shown to be an index, an `Int`: a skolem or an unsolved
    /// variable by one of its (upper) bounds, and a `Keys` projection by its
    /// schema's shape if its items' types are unsolved
    pub(super) fn index_key(&self, key: Term) -> Result<bool, Residual> {
        self.spend()?;
        let Some(int) = self.db.intrinsic(Intrinsic::Int) else {
            return Ok(false);
        };
        match self.resolve(key)? {
            Term::Skolem(id) => {
                for (bound, _) in self.abstract_bounds(Abstract::Skolem(id)) {
                    if self.index_key(bound)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Term::View(view) => {
                let key = match self.reify(Term::View(view)) {
                    Ok(key) => key,
                    Err(Residual::Inference) => match self.keys_by_shape(view)? {
                        Some(key) => key,
                        None => return Ok(false),
                    },
                    Err(_) => return Ok(false),
                };
                Ok(self.probe(key, int)? == Status::Proven)
            }
            // Every solution is below each upper bound
            Term::Infer(id) => {
                for bound in self.bounds(id).upper() {
                    if self.index_key(bound)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// A lone `IndexItem` projection in `view` evaluated by the shape of its
    /// schema, whose item type may be unsolved: a schema of one repeated
    /// position gives that position's type for every index.
    fn item_by_shape(&self, view: TypeView) -> Result<Option<Term>, Residual> {
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(None);
        };
        let &[UnionMember::IndexItem(schema, key)] = &members[..] else {
            return Ok(None);
        };
        if !self.index_key(view.child(key))? {
            return Ok(None);
        }
        let Term::View(schema) = self.resolve(view.child(schema))? else {
            return Ok(None);
        };
        let Type::Schema(items) = self.db.ty(schema.ty) else {
            return Ok(None);
        };
        Ok(match &items[..] {
            [
                SchemaItem {
                    multiplicity: Multiplicity::Repeated,
                    element: Element::Positional(item),
                },
            ] => Some(schema.child(*item)),
            _ => None,
        })
    }

    /// A closed union with each projection's schema exposed (see
    /// [`Self::exposed_schema`]), or `None` if none has a transparent
    /// declaration to expose
    fn exposed_projections(&self, members: &[UnionMember]) -> Result<Option<TypeId>, Issue> {
        let mut changed = false;
        let mut exposed = Vec::with_capacity(members.len());
        for &member in members {
            let variance = member.intrinsic().map_or(Variance::Invariant, |intrinsic| {
                self.schema_variance(intrinsic)
            });
            match member
                .projected()
                .map(|schema| self.exposed_schema(schema, variance))
            {
                Some(Ok(Some(schema))) => {
                    changed = true;
                    exposed.push(member.with(schema));
                }
                // A projection whose schema can't be exposed stays as it is
                Some(Ok(None) | Err(Issue::Residual(_))) | None => exposed.push(member),
                Some(Err(issue)) => return Err(issue),
            }
        }
        Ok(changed.then(|| self.db.intern(Type::Union(exposed.into()))))
    }

    /// A projection member of the union `view` widened by one step, as a union
    /// of it alone, with the step that widens it: a rigid member to its first
    /// bound, a member schema's projection to its target's bound's (see
    /// [`Self::widened_fields`]), an item's rigid key toward its bound (see
    /// [`Self::widened_key`]), or its skolem key to its one closed bound, or to
    /// `Int` if a bound shows it's an index. `None` if it can't be widened.
    fn widened_projection(
        &self,
        view: TypeView,
        member: UnionMember,
    ) -> Result<Option<(Term, Step)>, Issue> {
        let union = |member: UnionMember| self.db.intern(Type::Union(vec![member].into()));
        match member {
            UnionMember::Type(_) => Ok(None),
            _ if self.rigid(member.id())?.is_some() => {
                let Some(&(bound, _)) = self.rigid_bounds(member.id()).first() else {
                    return Err(
                        Residual::Unsupported("a projection of a rigid without a bound").into(),
                    );
                };
                // The key may refer to the view's binders
                Ok(Some((
                    view.child(union(member.with(bound))),
                    Step::RigidBound,
                )))
            }
            UnionMember::IndexItem(schema, _)
            | UnionMember::Expand(schema)
            | UnionMember::Keys(schema)
            | UnionMember::Values(schema)
            | UnionMember::Entries(schema)
                if let Some((fields, step)) =
                    self.widened_fields(view.child(schema), FieldAccess::Read)? =>
            {
                Ok(Some((view.child(union(member.with(fields))), step)))
            }
            UnionMember::IndexItem(schema, key) => {
                // A skolem key widens to its one closed bound, which selects as
                // much. One shown to be an index by a bound selects only
                // positions, which `Int` selects all of.
                if let Term::Skolem(id) = self.resolve(view.child(key))? {
                    let bounds = self.abstract_bounds(Abstract::Skolem(id));
                    if let [(bound, step)] = &bounds[..]
                        && let Ok(bound) = self.reify(*bound)
                    {
                        let projected = union(UnionMember::IndexItem(schema, bound));
                        return Ok(Some((view.child(projected), step.clone())));
                    }
                    let Some(int) = self.db.intrinsic(Intrinsic::Int) else {
                        return Ok(None);
                    };
                    for (bound, step) in bounds {
                        if self.index_key(bound)? {
                            let projected = union(UnionMember::IndexItem(schema, int));
                            return Ok(Some((view.child(projected), step)));
                        }
                    }
                    return Ok(None);
                }
                Ok(self.widened_key(key)?.map(|(key, step)| {
                    let projected = union(UnionMember::IndexItem(schema, key));
                    (view.child(projected), step)
                }))
            }
            _ => Ok(None),
        }
    }

    /// An item projection's key widened by one step: a rigid member to its
    /// first bound, or the keys of a rigid schema to the keys of its first
    /// bound, with the step that reduces the rigid
    fn widened_key(&self, key: TypeId) -> Result<Option<(TypeId, Step)>, Issue> {
        let members = match self.db.ty(key) {
            Type::Union(members) => members.to_vec(),
            _ => vec![UnionMember::Type(key)],
        };
        let mut widened = Vec::new();
        let mut step = None;
        for member in members {
            let bound = match member {
                UnionMember::Type(ty) | UnionMember::Keys(ty)
                    if step.is_none() && self.rigid(ty)?.is_some() =>
                {
                    self.rigid_bounds(ty).first().cloned()
                }
                _ => None,
            };
            let Some((bound, reduced)) = bound else {
                widened.push(member);
                continue;
            };
            step = Some(reduced);
            match (member, self.db.ty(bound)) {
                (UnionMember::Type(_), Type::Union(members)) => widened.extend(members.iter()),
                (UnionMember::Type(_), _) => widened.push(UnionMember::Type(bound)),
                _ => widened.push(UnionMember::Keys(bound)),
            }
        }
        Ok(step.map(|step| (self.db.intern(Type::Union(widened.into())), step)))
    }

    /// A stuck union of projections in `view` lowered by one step (see the
    /// module documentation), or `None` if no member lowers. A write through
    /// a rigid's member schema, or a key of it, is lowered first: what its
    /// bound's member schema sets is set by every subtype's, as the override
    /// rules keep a setter from narrowing, so a value its bound's assignment
    /// takes fits, at a key it sets (see [`Self::widened_fields`]).
    /// `AssignItem` is invariant in any other schema, so only a member schema
    /// lowers. Otherwise a key of a rigid's schema lowers to the keys its
    /// bound is known to admit: a rigid schema is any schema below its bound,
    /// which may omit every item the bound doesn't require, so those are the
    /// keys of the bound's required literal items, and the index of each fixed
    /// position. A read schema of a rigid's class lists every member its bound
    /// does, which are required. They're only some of the keys it admits, so
    /// they're only sufficient.
    fn lowered_projection(
        &self,
        view: TypeView,
        members: &[UnionMember],
    ) -> Result<Option<Evaluation>, Issue> {
        let mut written = Vec::with_capacity(members.len());
        let mut reduced = None;
        for &member in members {
            if let UnionMember::AssignItem(schema, _) | UnionMember::Keys(schema) = member
                && let Some((fields, step)) =
                    self.widened_fields(view.child(schema), FieldAccess::Write)?
            {
                reduced.get_or_insert(step);
                written.push(member.with(fields));
            } else {
                written.push(member);
            }
        }
        if let Some(step) = reduced {
            // The keys may refer to the view's binders
            let written = view.child(self.db.intern(Type::Union(written.into())));
            return Ok(Some(Evaluation::Lower(written, step)));
        }
        let mut known = Vec::with_capacity(members.len());
        let mut changed = false;
        for &member in members {
            let UnionMember::Keys(schema) = member else {
                known.push(member);
                continue;
            };
            let schema = view.child(schema);
            let bound = match self.widened_fields(schema, FieldAccess::Read)? {
                Some((fields, _)) => Some(fields),
                None => self.closed_abstract_bound(schema)?.map(|(bound, _)| bound),
            };
            let Some(bound) = bound else {
                known.push(member);
                continue;
            };
            let promoted = match self.keyed_view(bound, self.schema_variance(Intrinsic::Keys)) {
                Ok(Some(promoted)) => promoted,
                Ok(None) | Err(Issue::Residual(_)) => return Ok(None),
                Err(issue) => return Err(issue),
            };
            changed = true;
            known.extend((0..promoted.fixed.len()).map(|index| {
                let index = Literal::Int(index as i128);
                UnionMember::Type(self.db.intern(Type::Literal(index)))
            }));
            known.extend(
                promoted
                    .keyed
                    .iter()
                    .filter(|&&(multiplicity, key, _)| {
                        multiplicity == Multiplicity::Required && self.db.literal(key).is_some()
                    })
                    .map(|&(_, key, _)| UnionMember::Type(key)),
            );
        }
        Ok(changed.then(|| {
            let known = view.child(self.db.intern(Type::Union(known.into())));
            Evaluation::Sufficient(known)
        }))
    }

    /// The values of a closed `AssignItem`, `InsertItem` or `PushItem` that
    /// `view` is, where its key selects more than one, each with the step that
    /// selects it. It's their meet, and a write must fit each, which needs no
    /// meet of them. `None` if `view` isn't one, or isn't closed.
    pub(super) fn write_selection(
        &self,
        view: TypeView,
    ) -> Result<Option<Vec<(Term, Step)>>, Issue> {
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(None);
        };
        let selected = |values: Vec<TypeId>| {
            let mut distinct = Vec::new();
            for value in values {
                if !distinct.contains(&value) {
                    distinct.push(value);
                }
            }
            (distinct.len() > 1).then(|| {
                distinct
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| (self.closed(value), Step::Selected(index)))
                    .collect()
            })
        };
        if let &[UnionMember::PushItem(schema)] = &members[..] {
            let Ok(schema) = self.reify(view.child(schema)) else {
                return Ok(None);
            };
            let Ok(pushed) = self.pushed(schema) else {
                return Ok(None);
            };
            return Ok(selected(pushed));
        }
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
        let variance = member.intrinsic().map_or(Variance::Invariant, |intrinsic| {
            self.schema_variance(intrinsic)
        });
        let Ok(values) = self.selected(schema, key, variance) else {
            return Ok(None);
        };
        Ok(selected(values))
    }
}
