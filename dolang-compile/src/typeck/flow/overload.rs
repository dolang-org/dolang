//! Checking an overloaded def's body under each of its `@def` overloads, as well
//! as under its implementation's signature.
//!
//! A variant is a run analyzed again with its root's declared type taken from
//! one overload. Its parameter list is walked against the overload's argument
//! schema as a pattern ([`Solver::bind_schema`]), so overloads that differ in
//! arity need nothing special: an absent parameter takes its default's own type,
//! and a present one skips its default. What a variant reports that the
//! implementation's run didn't is reported under the overloads that found it.

use super::{At, Flow, Problem, Results, State, declared_signature};
use crate::{
    diag::{INVALID_SPAN, Span},
    typeck::{
        cfg::{FuncId, FuncKind, Ir, Pattern, PatternItem, PatternKey, Run, VarId},
        elab::{DeclNode, Tables},
        report::Report,
        solver::{Bound, Gas, Presence, Reason, Residual},
        r#type::{Database, Element, Multiplicity, SchemaItem, Type, TypeId},
    },
};

/// The overload a variant checks its run's root under
pub(super) struct Variant {
    pub(super) func: FuncId,
    /// The overload's name, which its problems point to
    pub(super) span: Span,
    /// Whether each parameter's slot is filled, by item index
    pub(super) presence: Vec<Presence>,
}

/// The `@def` overloads a run's root is also checked under, by signature, with
/// each one's name: none unless the root is a def or method with an
/// implementation whose body isn't a stub
pub(super) fn overloads(ir: &Ir, tables: &Tables<'_>, run: &Run) -> Vec<(usize, Span)> {
    let func = ir.func(run.root);
    let FuncKind::Decl(decl) = func.kind else {
        return Vec::new();
    };
    if func.stub {
        return Vec::new();
    }
    let Some(decl) = tables.try_decl(decl) else {
        return Vec::new();
    };
    let Some(primary) = decl.node.primary_sig() else {
        return Vec::new();
    };
    match &decl.node {
        DeclNode::Defs(defs) => defs
            .iter()
            .enumerate()
            .filter(|&(sig, def)| sig != primary && def.type_only)
            .map(|(sig, def)| (sig, def.name.span))
            .collect(),
        DeclNode::Methods(methods) => methods
            .iter()
            .enumerate()
            .filter(|&(sig, method)| sig != primary && method.overload)
            .map(|(sig, method)| (sig, method.name.span))
            .collect(),
        _ => Vec::new(),
    }
}

/// Fold what each variant reported into the implementation's run's `results`:
/// a problem at a span where it reported none is reported once, under every
/// overload that found it, and an undecided check at a span where it has
/// neither is recorded. An overload's types are usually below the
/// implementation's, so what the implementation's run reports at a span covers
/// what a variant finds there, in other words.
pub(super) fn fold(results: &mut Results, variants: Vec<(Span, Results)>) {
    // An unchecked cast is needless only if it is under every overload too
    results.problems.retain(|problem| {
        !matches!(problem, Problem::Assertion { .. })
            || variants
                .iter()
                .all(|(_, variant)| variant.problems.contains(problem))
    });
    let reported: Vec<Span> = results.problems.iter().map(Report::span).collect();
    let mut found: Vec<(Problem, Vec<Span>)> = Vec::new();
    for (span, variant) in variants {
        for problem in variant.problems {
            if reported.contains(&problem.span()) || matches!(problem, Problem::Assertion { .. }) {
                continue;
            }
            // Problems that aren't about the body, such as an uncovered overload,
            // are reported as they are
            if matches!(problem, Problem::OverloadUncovered { .. }) {
                found.push((problem, Vec::new()));
                continue;
            }
            match found.iter_mut().find(|(other, _)| *other == problem) {
                Some((_, spans)) => spans.push(span),
                None => found.push((problem, vec![span])),
            }
        }
        for unresolved in variant.unresolved {
            if !reported.contains(&unresolved.0) && !results.unresolved.contains(&unresolved) {
                results.unresolved.push(unresolved);
            }
        }
    }
    for (problem, overloads) in found {
        results.problems.push(match overloads.is_empty() {
            true => problem,
            false => Problem::UnderOverload {
                problem: Box::new(problem),
                overloads,
            },
        });
    }
    super::sort(&mut results.problems, &mut results.unresolved);
}

impl Flow<'_, '_> {
    /// Check the run's root under its overload `sig`, named at `span`, rather
    /// than its implementation's signature
    pub(super) fn vary(&mut self, db: &Database, sig: usize, span: Span) {
        let func = self.run.root;
        let FuncKind::Decl(decl) = self.ir.func(func).kind else {
            unreachable!("only a def's run varies")
        };
        self.declared[func.index()] = declared_signature(db, self.tables, decl, sig);
        let key = (decl, sig);
        for binder in self.tables.group(key).into_iter().flatten() {
            let key = (binder.decl, binder.sig);
            let owner = self
                .tables
                .sig_decl(key)
                .expect("every signature is declared");
            if !self.scope.iter().any(|(decl, _)| *decl == owner) {
                self.scope
                    .push((owner, self.tables.group_rigids(self.db, key)));
            }
        }
        self.variant = Some(Variant {
            func,
            span,
            presence: Vec::new(),
        });
    }

    /// Bind a variant's root's parameters from its overload's argument schema,
    /// walking them as a pattern. `false` if they can't take any call it admits,
    /// which is reported, or can't be walked: then its body isn't checked.
    pub(super) fn bind_overload(&mut self, at: At, state: &mut State) -> bool {
        let func = self.ir.func(at.func);
        let Pattern::Unpack { items, .. } = &func.params else {
            unreachable!("parameters are a pattern of items")
        };
        let Some(declared) = self.declared[at.func.index()].as_ref() else {
            return false;
        };
        let schema = declared.params;
        let shape = self.pattern_shape(at, state, items);
        let gas = Gas::default();
        let span = self.variant.as_ref().expect("a variant").span;
        let Some(bound) = self.solver(&gas).bind_schema(schema, &shape) else {
            // The body goes unchecked under the overload, which is undecided
            self.undecided(
                span,
                Residual::Unsupported(
                    "an overload whose arguments the parameters can't be bound from",
                ),
            );
            return false;
        };
        if let Some(reason) = bound.rejection {
            let (missing, example) = match reason {
                Reason::Fewer(count) => (true, format!("passes {}", positionals(count))),
                Reason::More(count) => (false, format!("passes {}", positionals(count))),
                Reason::Lacks(key) => (true, format!("doesn't pass `{}`", self.shown(key))),
                Reason::Extra(key) => (false, format!("passes `{}`", self.shown(key))),
            };
            self.problem(Problem::OverloadUncovered {
                span,
                missing,
                example,
            });
        }
        if !bound.possible {
            return false;
        }
        let (types, presence) = self.item_types(items, shape.positional.len(), &bound);
        self.variant.as_mut().expect("a variant").presence = presence;
        for (item, ty) in items.iter().zip(types) {
            if let Some(var) = item.var {
                self.binding(at, state, var, ty, INVALID_SPAN);
            }
        }
        true
    }

    /// A bound parameter list's types and presence, in the order of its items.
    /// A rest binds a `Tuple` or `Pack` of what it takes.
    fn item_types(
        &self,
        items: &[PatternItem],
        positional: usize,
        bound: &Bound,
    ) -> (Vec<TypeId>, Vec<Presence>) {
        let mut next_positional = 0;
        let mut next_keyed = positional;
        let mut next_rest = 0;
        let mut types = Vec::with_capacity(items.len());
        let mut presence = Vec::with_capacity(items.len());
        for item in items {
            match item.key {
                PatternKey::Pos => {
                    types.push(bound.slots[next_positional]);
                    presence.push(bound.presence[next_positional]);
                    next_positional += 1;
                }
                PatternKey::Key(_) | PatternKey::ConstKey(_) => {
                    types.push(bound.slots[next_keyed]);
                    presence.push(bound.presence[next_keyed]);
                    next_keyed += 1;
                }
                PatternKey::Rest(kind) => {
                    let schema = bound.rests[next_rest];
                    next_rest += 1;
                    let items = match self.db.ty(schema) {
                        Type::Schema(items) => items.to_vec(),
                        _ => vec![SchemaItem {
                            multiplicity: Multiplicity::Required,
                            element: Element::Include(schema),
                        }],
                    };
                    types.push(self.rest_type(kind, &items));
                    presence.push(Presence::Present);
                }
            }
        }
        (types, presence)
    }

    /// Whether a variant's root's parameter `var` is filled by every call its
    /// overload admits, by none, or by some; `None` outside a variant
    pub(super) fn presence(&self, at: At, var: VarId) -> Option<Presence> {
        let variant = self.variant.as_ref()?;
        if variant.func != at.func {
            return None;
        }
        let Pattern::Unpack { items, .. } = &self.ir.func(at.func).params else {
            return None;
        };
        let index = items.iter().position(|item| item.var == Some(var))?;
        variant.presence.get(index).copied()
    }
}

fn positionals(count: usize) -> String {
    match count {
        0 => "no positional arguments".to_owned(),
        1 => "1 positional argument".to_owned(),
        _ => format!("{count} positional arguments"),
    }
}
