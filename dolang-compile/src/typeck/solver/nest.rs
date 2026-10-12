//! What solvers nested in one another share (#900)

use std::cell::{Cell, RefCell};

use dolang_util::mono::{MonoHashSet, MonoVec};

use super::{
    Bounds, Dependency, Environment, EnvironmentId, InferVarId, ObligationId, ObligationLayer,
    Relation, Residual, Scope, ScopeId, Skolem, SkolemId, Solver, State, Step, Term, Variable,
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

/// An entry's index among a table's own, given where they begin
fn own(base: usize, id: usize) -> usize {
    id.checked_sub(base)
        .expect("an entry of the solver this one is nested in")
}

/// The tables, as the solver sees them through the layers of the solvers it's
/// nested in. Each layer records what its solver adds: entries it creates,
/// changes to the state of others, and additions to the sets that only grow.
impl Solver<'_> {
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

    /// What a variable is
    pub(super) fn variable(&self, id: InferVarId) -> &Variable {
        &self.variables[own(self.bases.variables, id.0)]
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

    /// The layer holding a variable's resolution as this solver sees it
    fn layer(&self, id: InferVarId) -> &VariableLayer {
        &self.layers[own(self.bases.variables, id.0)]
    }

    /// The layer this solver records changes to a variable in
    fn layer_mut(&self, id: InferVarId) -> &VariableLayer {
        self.writable();
        self.layer(id)
    }

    /// Visit each layer that records a variable, outermost first
    fn variable_layers(&self, id: InferVarId, visit: &mut impl FnMut(&VariableLayer)) {
        visit(self.layer(id));
    }

    /// A variable's resolution
    pub(super) fn state(&self, id: InferVarId) -> &VariableState {
        &self.layer(id).state
    }

    /// A variable's resolution, to change
    pub(super) fn state_mut(&self, id: InferVarId) -> &VariableState {
        &self.layer_mut(id).state
    }

    /// Mark a variable's bounds as possibly changed
    pub(super) fn dirty(&self, id: InferVarId) {
        self.dirty.borrow_mut()[id.0] = true;
    }

    /// Clear a variable's mark, returning whether it was marked
    pub(super) fn clean(&self, id: InferVarId) -> bool {
        std::mem::take(&mut self.dirty.borrow_mut()[id.0])
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
        self.relations[own(self.bases.obligations, id.0)]
    }

    /// The obligation judging a relation, if it's interned
    pub(super) fn interned(&self, relation: &Relation) -> Option<ObligationId> {
        self.obligation_index.get(relation).copied()
    }

    /// Intern a new obligation
    pub(super) fn push_obligation(&self, relation: Relation) -> ObligationId {
        self.writable();
        let id = ObligationId(self.obligation_count());
        self.relations.push(relation);
        self.obligations.push(ObligationLayer {
            state: Cell::new(State::Pending),
            queued: Cell::new(false),
            active: RefCell::new(Vec::new()),
            dependencies: MonoVec::new(),
        });
        self.obligation_index.try_insert(relation, id).unwrap();
        id
    }

    /// The layer holding an obligation's processing as this solver sees it
    pub(super) fn progress(&self, id: ObligationId) -> &ObligationLayer {
        &self.obligations[own(self.bases.obligations, id.0)]
    }

    /// The layer this solver records an obligation's processing in
    pub(super) fn progress_mut(&self, id: ObligationId) -> &ObligationLayer {
        self.writable();
        self.progress(id)
    }

    /// Visit each layer that records an obligation, outermost first
    fn obligation_layers(&self, id: ObligationId, visit: &mut impl FnMut(&ObligationLayer)) {
        visit(self.progress(id));
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

    /// Whether an environment exists
    pub(super) fn has_environment(&self, id: EnvironmentId) -> bool {
        id.0 < self.bases.environments + self.environments.len()
    }

    /// A substitution frame
    pub(super) fn frame(&self, id: EnvironmentId) -> &Environment {
        (self
            .environments
            .get_by_index(own(self.bases.environments, id.0)))
        .expect("an environment of this solver")
    }

    /// Intern a substitution frame
    pub(super) fn intern_frame(&self, environment: Environment) -> EnvironmentId {
        self.writable();
        EnvironmentId(self.bases.environments + self.environments.id_owned(environment).index())
    }

    /// How many skolems it sees
    pub(super) fn skolem_count(&self) -> usize {
        self.bases.skolems + self.skolems.len()
    }

    pub(super) fn skolem(&self, id: SkolemId) -> &Skolem {
        &self.skolems[own(self.bases.skolems, id.0)]
    }

    pub(super) fn push_skolem(&self, skolem: Skolem) -> SkolemId {
        self.writable();
        let id = SkolemId(self.skolem_count());
        self.skolems.push(skolem);
        id
    }

    /// A skolemization's extent
    pub(super) fn extent(&self, id: ScopeId) -> &Scope {
        &self.scopes[own(self.bases.scopes, id.0)]
    }

    pub(super) fn push_scope(&self, scope: Scope) -> ScopeId {
        self.writable();
        let id = ScopeId(self.bases.scopes + self.scopes.len());
        self.scopes.push(scope);
        id
    }

    /// The environment an obligation's instantiation created, if it has one
    pub(super) fn instantiation_of(&self, obligation: ObligationId) -> Option<EnvironmentId> {
        self.instantiations.borrow().get(&obligation).copied()
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
        self.skolemizations.borrow().get(&obligation).copied()
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
}
