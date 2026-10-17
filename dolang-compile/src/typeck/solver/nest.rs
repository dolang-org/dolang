//! What solvers nested in one another share (#900)

use std::{
    cell::{Cell, RefCell, RefMut},
    hash::Hash,
    iter,
};

use dolang_util::{
    intern,
    mono::{MonoHashMap, MonoHashSet, MonoVec},
};
use rustc_hash::{FxBuildHasher, FxHashMap};

use super::{
    Alternatives, Bases, Bounds, Constructor, DeclId, Dependency, Environment, EnvironmentId,
    GenericMember, InferVarId, Issue, Lookup, ObligationId, ObligationLayer, Relation, Residual,
    Root, Scope, ScopeId, Signature, Skolem, SkolemId, Solver, State, Step, Term, TypeId, Variable,
    VariableLayer, VariableState,
};

/// A work budget, shared by a solver and the solvers nested in it. Each unit of
/// work spends one; once spent, it stays exhausted.
pub(crate) struct Gas {
    limit: usize,
    used: Cell<usize>,
    exhausted: Cell<bool>,
}

impl Gas {
    /// The budget of a query whose caller has no reason to choose one
    pub(crate) const WORK: usize = 100_000;

    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            used: Cell::new(0),
            exhausted: Cell::new(false),
        }
    }

    /// Spend one unit, marking the budget exhausted if none is left
    pub(super) fn spend(&self) -> Result<(), Residual> {
        if self.used.get() >= self.limit {
            self.exhausted.set(true);
            return Err(Residual::Limit);
        }
        self.used.set(self.used.get() + 1);
        Ok(())
    }

    /// Run `f`, then refund what it spent, as rendering does
    #[cfg(feature = "debug")]
    pub(super) fn refunded<T>(&self, f: impl FnOnce() -> T) -> T {
        let (used, exhausted) = (self.used.get(), self.exhausted.get());
        let result = f();
        self.used.set(used);
        self.exhausted.set(exhausted);
        result
    }

    pub(super) fn exhaust(&self) {
        self.exhausted.set(true);
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted.get()
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    pub(crate) fn used(&self) -> usize {
        self.used.get()
    }
}

impl Default for Gas {
    fn default() -> Self {
        Self::new(Self::WORK)
    }
}

/// The solver a solver is nested in, and how. Only the accessors here reach
/// it, and nothing changes it while it's a parent.
pub(super) struct Parent<'a> {
    solver: &'a Solver<'a>,
    nesting: Nesting,
}

impl Drop for Parent<'_> {
    fn drop(&mut self) {
        self.solver.child.set(false);
    }
}

/// How a solver is nested in its parent. An open solver sees all its parent's
/// state; a closed one, only its environments and the caches of what its
/// assumptions imply, so it solves from nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Nesting {
    /// An open trial of an alternative (see [`Solver::trial`])
    Trial,
    /// An open solver its caller constrains (see [`Solver::fork`])
    Fork,
    /// A closed proof of `actual <: expected` (see [`Solver::probe`]). It's
    /// a premise of a judgment if `premise` is the judgment's guard bit (see
    /// [`Solver::guarded`]), or else a query whose answer steers a choice.
    Probe {
        actual: TypeId,
        expected: TypeId,
        premise: Option<bool>,
    },
    /// A closed solver for a side query, whose answers are queries'
    Side,
}

/// What a probe would be of a judgment a probe it's nested in is proving (see
/// [`Solver::revisit`])
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Revisit {
    /// It revisits none
    Fresh,
    /// It revisits one it may not assume
    Cycle,
    /// It revisits one it may assume
    Assumed,
}

impl Nesting {
    fn is_open(self) -> bool {
        matches!(self, Self::Trial | Self::Fork)
    }
}

impl<'db> Solver<'db> {
    /// A solver nested in this one, whose work is charged to `gas`. It records
    /// what it changes itself, so nothing solved on it reaches this one, which
    /// can't change while it's live. An open one processes what this one has
    /// queued, as this one would.
    pub(super) fn child<'a>(&'a self, nesting: Nesting, gas: &'a Gas) -> Solver<'a> {
        assert!(
            !self.child.replace(true),
            "a solver nested in one that already has a live one"
        );
        let open = nesting.is_open();
        let queue = MonoVec::new();
        if open {
            for &id in self.queue.iter() {
                queue.push(id);
            }
        }
        Solver {
            db: self.db,
            bases: Bases {
                variables: self.variable_count(),
                obligations: self.obligation_count(),
                environments: self.environment_count(),
                skolems: self.skolem_count(),
                scopes: self.scope_count(),
            },
            environments: intern::Table::new(),
            variables: MonoVec::new(),
            layers: MonoVec::new(),
            overlays: MonoHashMap::default(),
            first_variable: match open {
                true => self.first_variable,
                false => self.variable_count(),
            },
            dirty: RefCell::new(match open {
                true => self.dirty.borrow().clone(),
                false => Vec::new(),
            }),
            instantiations: RefCell::default(),
            skolemizations: RefCell::default(),
            skolems: MonoVec::new(),
            scopes: MonoVec::new(),
            relations: MonoVec::new(),
            obligations: MonoVec::new(),
            obligation_overlays: MonoHashMap::default(),
            obligation_index: MonoHashMap::default(),
            queue,
            roots: Vec::new(),
            gas,
            depth: self.depth,
            scope: self.scope.clone(),
            groups: self.groups.clone(),
            gradual: self.gradual,
            rigid_bounds: RefCell::default(),
            closed: open && self.closed,
            alternatives: RefCell::default(),
            constructors: RefCell::default(),
            default_constructions: RefCell::default(),
            generic_members: RefCell::default(),
            inherits: true,
            generation: Cell::new(self.generation.get()),
            trials_from: match open {
                true => self.trials_from,
                false => self.obligation_count(),
            },
            owned: self.owned.filter(|_| open),
            blinded: FxHashMap::default(),
            tracks: self.tracks || matches!(nesting, Nesting::Probe { .. }),
            parent: Some(Parent {
                solver: self,
                nesting,
            }),
            child: Cell::new(false),
            #[cfg(feature = "debug")]
            names: self.names,
            #[cfg(feature = "debug")]
            indent: self.indent + 1,
        }
    }

    /// The solver this one is nested in, if any
    fn parent(&self) -> Option<&'db Solver<'db>> {
        self.parent.as_ref().map(|parent| parent.solver)
    }

    /// The solver this one is nested in, if it's open
    fn open_parent(&self) -> Option<&'db Solver<'db>> {
        (self.parent.as_ref())
            .filter(|parent| parent.nesting.is_open())
            .map(|parent| parent.solver)
    }

    /// The solver this one is nested in, which holds the entries below its
    /// bases. Only an open solver sees those other than environments.
    fn outer(&self) -> &'db Solver<'db> {
        self.parent()
            .expect("an entry of a solver this one isn't nested in")
    }

    /// [`Self::outer`], for an entry only an open solver sees
    fn enclosing(&self) -> &'db Solver<'db> {
        self.open_parent()
            .expect("an entry a closed solver doesn't see")
    }

    /// Whether a probe of `actual <: expected` would revisit one it's nested
    /// in, and if so, whether it may assume what that one proves. `from` is the
    /// guard bit of the judgment it would be a premise of, if it would be one.
    ///
    /// The revisit is assumed only if a guarded step lies between the two
    /// (coinduction: #761's contractive aliases make this sound, and its
    /// regular ones keep the judgments a probe meets finite), and only if
    /// each probe between them is a premise. A query's answer may steer
    /// choices, which an assumption must not, and nothing proven under an
    /// assumption is cached outside the probe that makes it, since a solver
    /// writes only its own caches and goes with its probe.
    pub(super) fn revisit(&self, actual: TypeId, expected: TypeId, from: Option<bool>) -> Revisit {
        let mut guarded = from.unwrap_or(false);
        let mut premise = from.is_some();
        let links = iter::successors(self.parent.as_ref(), |parent| parent.solver.parent.as_ref());
        for link in links {
            match link.nesting {
                Nesting::Probe {
                    actual: a,
                    expected: b,
                    premise: link_premise,
                } => {
                    if (a, b) == (actual, expected) {
                        return match guarded && premise {
                            true => Revisit::Assumed,
                            false => Revisit::Cycle,
                        };
                    }
                    match link_premise {
                        Some(bit) => guarded |= bit,
                        None => premise = false,
                    }
                }
                Nesting::Side => premise = false,
                // An open solver's judgments are guarded from its parent's root
                Nesting::Trial | Nesting::Fork => {}
            }
        }
        Revisit::Fresh
    }

    /// How many trials it's nested in
    pub(super) fn trial_depth(&self) -> usize {
        iter::successors(self.parent.as_ref(), |parent| parent.solver.parent.as_ref())
            .filter(|parent| parent.nesting == Nesting::Trial)
            .count()
    }
}

/// The tables, as the solver sees them through the layers of the solvers it's
/// nested in. Each layer records what its solver adds: entries it creates,
/// changes to the state of others, and additions to the sets that only grow.
/// An entry below a table's base is its parent's.
impl<'db> Solver<'db> {
    /// Assert that no solver nested in this one is live, before changing it
    pub(super) fn writable(&self) {
        assert!(
            !self.child.get(),
            "a solver changed while a solver nested in it is live"
        );
    }

    /// How many variables it sees
    pub(super) fn variable_count(&self) -> usize {
        self.bases.variables + self.variables.len()
    }

    /// The variables it sees: a closed solver's are its own
    pub(super) fn variable_ids(&self) -> impl Iterator<Item = InferVarId> + use<> {
        (self.first_variable..self.variable_count()).map(InferVarId)
    }

    /// What a variable is
    pub(super) fn variable(&self, id: InferVarId) -> &Variable {
        match id.0.checked_sub(self.bases.variables) {
            Some(index) => &self.variables[index],
            None => self.enclosing().variable(id),
        }
    }

    /// A new variable
    pub(super) fn push_variable(&self, variable: Variable) -> InferVarId {
        self.writable();
        let id = InferVarId(self.variable_count());
        self.variables.push(variable);
        self.layers.push(VariableLayer::default());
        self.dirty.borrow_mut().push(false);
        id
    }

    /// The layer this solver records changes to a variable in, starting from
    /// its parent's resolution of it
    fn layer_mut(&self, id: InferVarId) -> &VariableLayer {
        self.writable();
        match id.0.checked_sub(self.bases.variables) {
            Some(index) => &self.layers[index],
            None => self.overlays.get_or_insert_with(&id.0, |&index| {
                let layer = VariableLayer {
                    state: self.enclosing().state(id).clone(),
                    ..VariableLayer::default()
                };
                (index, layer)
            }),
        }
    }

    /// Visit each layer that records a variable, outermost first
    fn variable_layers(&self, id: InferVarId, visit: &mut impl FnMut(&VariableLayer)) {
        match id.0.checked_sub(self.bases.variables) {
            Some(index) => visit(&self.layers[index]),
            None => {
                self.enclosing().variable_layers(id, visit);
                if let Some(layer) = self.overlays.get(&id.0) {
                    visit(layer);
                }
            }
        }
    }

    /// A variable's resolution
    pub(super) fn state(&self, id: InferVarId) -> &VariableState {
        match id.0.checked_sub(self.bases.variables) {
            Some(index) => &self.layers[index].state,
            None => match self.overlays.get(&id.0) {
                Some(layer) => &layer.state,
                None => self.enclosing().state(id),
            },
        }
    }

    /// A variable's resolution, to change
    pub(super) fn state_mut(&self, id: InferVarId) -> &VariableState {
        &self.layer_mut(id).state
    }

    /// Mark a variable's bounds as possibly changed
    pub(super) fn dirty(&self, id: InferVarId) {
        self.dirty.borrow_mut()[id.0 - self.first_variable] = true;
    }

    /// Clear a variable's mark, returning whether it was marked
    pub(super) fn clean(&self, id: InferVarId) -> bool {
        std::mem::take(&mut self.dirty.borrow_mut()[id.0 - self.first_variable])
    }

    pub(crate) fn bounds(&self, id: InferVarId) -> Bounds {
        let mut bounds = Bounds::default();
        self.variable_layers(id, &mut |layer| {
            for (lower, terms) in [(true, &mut bounds.lower), (false, &mut bounds.upper)] {
                // Layers record a term again only for a new source
                let first = terms.is_empty();
                for (&term, _) in layer.bounds.side(lower).iter() {
                    if first || !terms.contains(&term) {
                        terms.push(term);
                    }
                }
            }
        });
        bounds
    }

    /// One side of a variable's bounds, each with the obligations that imposed it
    pub(super) fn bound_sources(
        &self,
        id: InferVarId,
        lower: bool,
    ) -> Vec<(Term, Vec<ObligationId>)> {
        let mut found: Vec<(Term, Vec<ObligationId>)> = Vec::new();
        self.variable_layers(id, &mut |layer| {
            let first = found.is_empty();
            for (&term, sources) in layer.bounds.side(lower).iter() {
                let sources = sources.iter().copied();
                match (!first)
                    .then(|| found.iter_mut().find(|(known, _)| *known == term))
                    .flatten()
                {
                    Some((_, known)) => known.extend(sources),
                    None => found.push((term, sources.collect())),
                }
            }
        });
        found
    }

    /// The obligations that imposed a bound on a variable
    pub(super) fn term_sources(
        &self,
        id: InferVarId,
        lower: bool,
        term: Term,
    ) -> Vec<ObligationId> {
        let mut found = Vec::new();
        self.variable_layers(id, &mut |layer| {
            if let Some(sources) = layer.bounds.side(lower).get(&term) {
                found.extend(sources.iter().copied());
            }
        });
        found
    }

    /// Record that `source` imposes a bound on a variable, returning whether it
    /// hadn't already
    pub(super) fn insert_bound(
        &self,
        id: InferVarId,
        lower: bool,
        term: Term,
        source: ObligationId,
    ) -> bool {
        let mut known = false;
        self.variable_layers(id, &mut |layer| {
            known |= (layer.bounds.side(lower).get(&term))
                .is_some_and(|sources| sources.contains(&source));
        });
        !known
            && (self.layer_mut(id).bounds.side(lower))
                .get_or_insert_with(&term, |term| (*term, MonoHashSet::default()))
                .try_insert(source)
                .is_ok()
    }

    /// The obligations that supported a variable's assignment
    pub(super) fn support(&self, id: InferVarId) -> Vec<ObligationId> {
        let mut found = Vec::new();
        self.variable_layers(id, &mut |layer| found.extend(layer.support.iter().copied()));
        found
    }

    pub(super) fn add_support(&self, id: InferVarId, source: ObligationId) {
        let mut known = false;
        self.variable_layers(id, &mut |layer| known |= layer.support.contains(&source));
        if !known {
            let _ = self.layer_mut(id).support.try_insert(source);
        }
    }

    /// The obligations a variable's assignment wakes
    pub(super) fn subscribers(&self, id: InferVarId) -> Vec<ObligationId> {
        let mut found = Vec::new();
        self.variable_layers(id, &mut |layer| {
            found.extend(layer.subscribers.iter().copied())
        });
        found
    }

    pub(super) fn add_subscriber(&self, id: InferVarId, obligation: ObligationId) {
        let mut known = false;
        self.variable_layers(id, &mut |layer| {
            known |= layer.subscribers.contains(&obligation)
        });
        if !known {
            let _ = self.layer_mut(id).subscribers.try_insert(obligation);
        }
    }

    /// How many obligations it sees
    pub(super) fn obligation_count(&self) -> usize {
        self.bases.obligations + self.relations.len()
    }

    /// The relation an obligation judges
    pub(crate) fn relation(&self, id: ObligationId) -> Relation {
        self.judged(id).0
    }

    /// Whether an obligation was reached through a guarded step (see
    /// [`Solver::guards`]) since the root of the probe it's in. Outside a
    /// probe, none is.
    pub(super) fn guarded(&self, id: ObligationId) -> bool {
        self.judged(id).1
    }

    fn judged(&self, id: ObligationId) -> (Relation, bool) {
        match id.0.checked_sub(self.bases.obligations) {
            Some(index) => self.relations[index],
            None => self.enclosing().judged(id),
        }
    }

    /// The obligation judging a relation, reached guarded or not, if it's
    /// interned
    pub(super) fn interned(&self, key: &(Relation, bool)) -> Option<ObligationId> {
        (self.obligation_index.get(key).copied()).or_else(|| self.open_parent()?.interned(key))
    }

    /// Intern a new obligation
    pub(super) fn push_obligation(&self, key: (Relation, bool)) -> ObligationId {
        self.writable();
        let id = ObligationId(self.obligation_count());
        self.relations.push(key);
        self.obligations.push(ObligationLayer {
            state: Cell::new(State::Pending),
            queued: Cell::new(false),
            active: RefCell::new(Vec::new()),
            dependencies: MonoVec::new(),
        });
        self.obligation_index.try_insert(key, id).unwrap();
        id
    }

    /// The layer holding an obligation's processing as this solver sees it
    pub(super) fn progress(&self, id: ObligationId) -> &ObligationLayer {
        match id.0.checked_sub(self.bases.obligations) {
            Some(index) => &self.obligations[index],
            None => match self.obligation_overlays.get(&id.0) {
                Some(layer) => layer,
                None => self.enclosing().progress(id),
            },
        }
    }

    /// The layer this solver records an obligation's processing in, starting
    /// from its parent's
    pub(super) fn progress_mut(&self, id: ObligationId) -> &ObligationLayer {
        self.writable();
        match id.0.checked_sub(self.bases.obligations) {
            Some(index) => &self.obligations[index],
            None => self
                .obligation_overlays
                .get_or_insert_with(&id.0, |&index| {
                    let outer = self.enclosing().progress(id);
                    let layer = ObligationLayer {
                        state: outer.state.clone(),
                        queued: outer.queued.clone(),
                        active: outer.active.clone(),
                        dependencies: MonoVec::new(),
                    };
                    (index, layer)
                }),
        }
    }

    /// Visit each layer that records an obligation, outermost first
    fn obligation_layers(&self, id: ObligationId, visit: &mut impl FnMut(&ObligationLayer)) {
        match id.0.checked_sub(self.bases.obligations) {
            Some(index) => visit(&self.obligations[index]),
            None => {
                self.enclosing().obligation_layers(id, visit);
                if let Some(layer) = self.obligation_overlays.get(&id.0) {
                    visit(layer);
                }
            }
        }
    }

    /// The edges from an obligation to those derived from it, in the order
    /// they were recorded
    pub(crate) fn dependencies(&self, id: ObligationId) -> Vec<Dependency> {
        let mut found = Vec::new();
        self.obligation_layers(id, &mut |layer| {
            found.extend(layer.dependencies.iter().cloned());
        });
        found
    }

    /// Record an edge from `parent` to `child`, unless it's recorded
    pub(super) fn add_dependency(&self, parent: ObligationId, child: ObligationId, step: Step) {
        let mut known = false;
        self.obligation_layers(parent, &mut |layer| {
            known |= (layer.dependencies.iter()).any(|d| d.obligation == child && d.step == step);
        });
        if !known {
            (self.progress_mut(parent).dependencies).push(Dependency {
                obligation: child,
                step,
            });
        }
    }

    /// How many environments it sees
    fn environment_count(&self) -> usize {
        self.bases.environments + self.environments.len()
    }

    /// Whether an environment exists
    pub(super) fn has_environment(&self, id: EnvironmentId) -> bool {
        id.0 < self.environment_count()
    }

    /// A substitution frame
    pub(super) fn frame(&self, id: EnvironmentId) -> &Environment {
        match id.0.checked_sub(self.bases.environments) {
            Some(index) => {
                (self.environments.get_by_index(index)).expect("an environment of this solver")
            }
            None => self.outer().frame(id),
        }
    }

    /// A substitution frame's ID, if it's interned
    fn find_frame(&self, environment: &Environment) -> Option<EnvironmentId> {
        (self.environments.get(environment))
            .map(|id| EnvironmentId(self.bases.environments + id.index()))
            .or_else(|| self.parent()?.find_frame(environment))
    }

    /// Intern a substitution frame
    pub(super) fn intern_frame(&self, environment: Environment) -> EnvironmentId {
        self.writable();
        if let Some(id) = self
            .parent()
            .and_then(|parent| parent.find_frame(&environment))
        {
            return id;
        }
        EnvironmentId(self.bases.environments + self.environments.id_owned(environment).index())
    }

    /// How many skolems it sees
    pub(super) fn skolem_count(&self) -> usize {
        self.bases.skolems + self.skolems.len()
    }

    pub(super) fn skolem(&self, id: SkolemId) -> &Skolem {
        match id.0.checked_sub(self.bases.skolems) {
            Some(index) => &self.skolems[index],
            None => self.enclosing().skolem(id),
        }
    }

    pub(super) fn push_skolem(&self, skolem: Skolem) -> SkolemId {
        self.writable();
        let id = SkolemId(self.skolem_count());
        self.skolems.push(skolem);
        id
    }

    /// How many skolemization scopes it sees
    fn scope_count(&self) -> usize {
        self.bases.scopes + self.scopes.len()
    }

    /// A skolemization's extent. A closed solver sees its parent's, which
    /// include the root scope.
    pub(super) fn extent(&self, id: ScopeId) -> &Scope {
        match id.0.checked_sub(self.bases.scopes) {
            Some(index) => &self.scopes[index],
            None => self.outer().extent(id),
        }
    }

    pub(super) fn push_scope(&self, scope: Scope) -> ScopeId {
        self.writable();
        let id = ScopeId(self.scope_count());
        self.scopes.push(scope);
        id
    }

    /// The environment an obligation's instantiation created, if it has one
    pub(super) fn instantiation_of(&self, obligation: ObligationId) -> Option<EnvironmentId> {
        (self.instantiations.borrow().get(&obligation).copied())
            .or_else(|| self.open_parent()?.instantiation_of(obligation))
    }

    pub(super) fn record_instantiation(
        &self,
        obligation: ObligationId,
        environment: EnvironmentId,
    ) {
        self.writable();
        self.instantiations
            .borrow_mut()
            .insert(obligation, environment);
    }

    /// The environment of skolems an obligation's skolemization created, if it
    /// has one
    pub(super) fn skolemization_of(&self, obligation: ObligationId) -> Option<EnvironmentId> {
        (self.skolemizations.borrow().get(&obligation).copied())
            .or_else(|| self.open_parent()?.skolemization_of(obligation))
    }

    pub(super) fn record_skolemization(
        &self,
        obligation: ObligationId,
        environment: EnvironmentId,
    ) {
        self.writable();
        self.skolemizations
            .borrow_mut()
            .insert(obligation, environment);
    }

    /// What one of the caches of what the solver's assumptions imply holds for
    /// `key`. A solver nested in another makes its assumptions until it makes
    /// its own, and sees its caches until then.
    pub(super) fn cached<K: Hash + Eq, V: Clone>(
        &self,
        cache: for<'s> fn(&'s Solver<'db>) -> &'s RefCell<FxHashMap<K, V>>,
        key: &K,
    ) -> Option<V> {
        if let Some(value) = cache(self).borrow().get(key) {
            return Some(value.clone());
        }
        if !self.inherits {
            return None;
        }
        self.parent()?.cached(cache, key)
    }

    /// Read the record of a judgment's alternatives, if it has one
    pub(super) fn alternative_record<R>(
        &self,
        obligation: ObligationId,
        read: impl FnOnce(&Alternatives) -> R,
    ) -> Option<R> {
        if let Some(record) = self.alternatives.borrow().get(&obligation) {
            return Some(read(record));
        }
        self.open_parent()?.alternative_record(obligation, read)
    }

    /// Whether a judgment has alternatives
    pub(super) fn has_alternatives(&self, obligation: ObligationId) -> bool {
        self.alternative_record(obligation, |_| ()).is_some()
    }

    pub(super) fn add_alternatives(&self, obligation: ObligationId, record: Alternatives) {
        self.writable();
        self.alternatives.borrow_mut().insert(obligation, record);
    }

    /// The record of a judgment's alternatives, to change, starting from its
    /// parent's
    pub(super) fn alternative_record_mut(
        &self,
        obligation: ObligationId,
    ) -> RefMut<'_, Alternatives> {
        self.writable();
        RefMut::map(self.alternatives.borrow_mut(), |records| {
            records.entry(obligation).or_insert_with(|| {
                (self
                    .enclosing()
                    .alternative_record(obligation, Alternatives::clone))
                .expect("a judgment with alternatives")
            })
        })
    }

    /// The judgments from `from` on that have alternatives
    pub(super) fn alternative_ids(&self, from: usize) -> Vec<ObligationId> {
        let records = self.alternatives.borrow();
        let mut ids: Vec<ObligationId> = (records.keys().copied())
            .filter(|id| id.0 >= from)
            .collect();
        if from < self.bases.obligations
            && let Some(parent) = self.open_parent()
        {
            ids.extend(
                (parent.alternative_ids(from).into_iter()).filter(|id| !records.contains_key(id)),
            );
        }
        ids
    }

    /// What trials judge in place of `term`, if anything (see [`Solver::blind`])
    pub(super) fn blinded(&self, term: Term) -> Option<Term> {
        (self.blinded.get(&term).copied()).or_else(|| self.open_parent()?.blinded(term))
    }
}

/// What a fork records, taken from it by [`Solver::commit`] for its parent to
/// absorb (see [`Solver::absorb`])
pub(crate) struct Delta {
    bases: Bases,
    environments: intern::Table<Environment, EnvironmentId>,
    variables: MonoVec<Variable>,
    layers: MonoVec<VariableLayer>,
    overlays: MonoHashMap<usize, VariableLayer, FxBuildHasher>,
    dirty: Vec<bool>,
    instantiations: FxHashMap<ObligationId, EnvironmentId>,
    skolemizations: FxHashMap<ObligationId, EnvironmentId>,
    skolems: MonoVec<Skolem>,
    scopes: MonoVec<Scope>,
    relations: MonoVec<(Relation, bool)>,
    obligations: MonoVec<ObligationLayer>,
    obligation_overlays: MonoHashMap<usize, ObligationLayer, FxBuildHasher>,
    queue: MonoVec<ObligationId>,
    roots: Vec<Root>,
    alternatives: FxHashMap<ObligationId, Alternatives>,
    blinded: FxHashMap<Term, Term>,
    /// The caches of what its assumptions imply, if they're its parent's
    caches: Option<Caches>,
    generation: usize,
}

/// A solver's caches of what its assumptions imply
struct Caches {
    rigid_bounds: FxHashMap<TypeId, Vec<(TypeId, Step)>>,
    constructors: FxHashMap<(DeclId, bool), Constructor>,
    default_constructions: FxHashMap<DeclId, Option<Option<Signature>>>,
    generic_members: FxHashMap<GenericMember, Result<Lookup, Issue>>,
}

impl<'db> Solver<'db> {
    /// What this fork records, for its parent to absorb (see
    /// [`Solver::absorb`]). It ends the fork, so its parent can change again.
    pub(crate) fn commit(self) -> Delta {
        let Solver {
            bases,
            environments,
            variables,
            layers,
            overlays,
            dirty,
            instantiations,
            skolemizations,
            skolems,
            scopes,
            relations,
            obligations,
            obligation_overlays,
            queue,
            roots,
            rigid_bounds,
            alternatives,
            constructors,
            default_constructions,
            generic_members,
            inherits,
            generation,
            blinded,
            parent,
            ..
        } = self;
        let parent = parent.expect("a fork to commit");
        assert_eq!(parent.nesting, Nesting::Fork, "only a fork commits");
        drop(parent);
        let caches = inherits.then(|| Caches {
            rigid_bounds: rigid_bounds.into_inner(),
            constructors: constructors.into_inner(),
            default_constructions: default_constructions.into_inner(),
            generic_members: generic_members.into_inner(),
        });
        Delta {
            bases,
            environments,
            variables,
            layers,
            overlays,
            dirty: dirty.into_inner(),
            instantiations: instantiations.into_inner(),
            skolemizations: skolemizations.into_inner(),
            skolems,
            scopes,
            relations,
            obligations,
            obligation_overlays,
            queue,
            roots,
            alternatives: alternatives.into_inner(),
            blinded,
            caches,
            generation: generation.get(),
        }
    }

    /// Take over what a fork of this one recorded, as if this one had done it:
    /// its entries keep their IDs, and its changes to this one's entries apply
    /// here. Nothing may have changed this one since the fork. Its constraints
    /// follow this one's, so [`Solver::solve`] gives their outcomes after this
    /// one's, though the IDs the fork gave them count from its own first.
    pub(crate) fn absorb(&mut self, delta: Delta) {
        let Delta {
            bases,
            environments,
            mut variables,
            mut layers,
            overlays,
            dirty,
            instantiations,
            skolemizations,
            mut skolems,
            mut scopes,
            mut relations,
            mut obligations,
            obligation_overlays,
            queue,
            roots,
            alternatives,
            blinded,
            caches,
            generation,
        } = delta;
        assert!(
            bases.variables == self.variable_count()
                && bases.obligations == self.obligation_count()
                && bases.environments == self.environment_count()
                && bases.skolems == self.skolem_count()
                && bases.scopes == self.scope_count(),
            "a fork absorbed into a solver changed since"
        );
        // Changes to this one's entries first, while its tables are as the
        // fork saw them
        for (&index, layer) in overlays.iter() {
            let id = InferVarId(index);
            let state = self.state_mut(id);
            state.assignment.set(layer.state.assignment.get());
            state.defaulted.set(layer.state.defaulted.get());
            state.exact.set(layer.state.exact.get());
            state.fallback.set(layer.state.fallback.get());
            for lower in [true, false] {
                for (&term, sources) in layer.bounds.side(lower).iter() {
                    for &source in sources.iter() {
                        self.insert_bound(id, lower, term, source);
                    }
                }
            }
            for &source in layer.support.iter() {
                self.add_support(id, source);
            }
            for &obligation in layer.subscribers.iter() {
                self.add_subscriber(id, obligation);
            }
        }
        for (&index, layer) in obligation_overlays.iter() {
            let id = ObligationId(index);
            let progress = self.progress_mut(id);
            progress.state.set(layer.state.get());
            progress.queued.set(layer.queued.get());
            *progress.active.borrow_mut() = layer.active.borrow().clone();
            for dependency in layer.dependencies.iter() {
                self.add_dependency(id, dependency.obligation, dependency.step.clone());
            }
        }
        for index in 0..environments.len() {
            let environment = environments.get_by_index(index).expect("a frame");
            let id = self.environments.id_owned(environment.clone());
            debug_assert_eq!(
                self.bases.environments + id.index(),
                bases.environments + index
            );
        }
        for (variable, layer) in variables.drain().zip(layers.drain()) {
            self.variables.push(variable);
            self.layers.push(layer);
        }
        // An open fork's marks start from this one's
        *self.dirty.get_mut() = dirty;
        for skolem in skolems.drain() {
            self.skolems.push(skolem);
        }
        for scope in scopes.drain() {
            self.scopes.push(scope);
        }
        for (key, layer) in relations.drain().zip(obligations.drain()) {
            let id = ObligationId(self.obligation_count());
            self.relations.push(key);
            self.obligations.push(layer);
            self.obligation_index.try_insert(key, id).unwrap();
        }
        // An open fork's queue starts from this one's
        self.queue = queue;
        self.roots.extend(roots);
        self.instantiations.get_mut().extend(instantiations);
        self.skolemizations.get_mut().extend(skolemizations);
        self.alternatives.get_mut().extend(alternatives);
        self.blinded.extend(blinded);
        if let Some(caches) = caches {
            self.rigid_bounds.get_mut().extend(caches.rigid_bounds);
            self.constructors.get_mut().extend(caches.constructors);
            (self.default_constructions.get_mut()).extend(caches.default_constructions);
            self.generic_members
                .get_mut()
                .extend(caches.generic_members);
        }
        self.generation.set(generation);
    }

    /// Run `f` on a fork of this one whose work is charged to `gas`, and take
    /// over what it records if `f` says to keep it
    #[cfg(test)]
    pub(crate) fn nest<R>(&mut self, gas: &Gas, f: impl FnOnce(&mut Solver<'_>) -> (R, bool)) -> R {
        let (result, delta) = {
            let mut fork = self.fork(gas);
            let (result, keep) = f(&mut fork);
            (result, keep.then(|| fork.commit()))
        };
        if let Some(delta) = delta {
            self.absorb(delta);
        }
        result
    }

    /// Whether a variable is this solver's own, rather than one it sees in
    /// the solver it's nested in
    pub(crate) fn owns(&self, id: InferVarId) -> bool {
        id.0 >= self.bases.variables
    }
}
