//! Well-formedness of the sealed database's declarations.
//!
//! Every check is local: it holds the binders of the declaration it is written in
//! as rigids, assuming only their bounds, and every application of a declaration
//! is checked against that declaration's bounds. So if every check passes, every
//! assumption is backed by one, and an invalid database can never validate
//! whatever order the checks run in. No verdict is cached or fed into another
//! check. A check the solver can't decide doesn't pass: it is returned as
//! unresolved. A recovery stand-in is `Unknown`, so checks through it pass, but
//! its site was already diagnosed.
//!
//! Written types are checked where they are written, to point diagnostics at the
//! offending application: each type argument, including defaults and variadic
//! items, against its binder's bound; each function type's parameters for symbol
//! keys, except within a `Phantom` argument, which only marks variance. An
//! ambient channel may be any type; reading or writing it is checked where that
//! happens. A declaration's own signature and binder defaults are checked as
//! declared. Recursion among transparent aliases must be contractive and regular.

use std::collections::{HashMap, HashSet};

use super::{
    BadRecursion, BoundViolation, DeclNode, Designated, Diag, Fill, Head, ParameterKeys, Referent,
    Role, Tables, UnitDiag,
    surface::{Class, Name, TypeArg, TypeExpr, TypeParam, TypeParamKind},
};
use crate::{
    source::Span,
    typeck::{
        report::Report,
        solver::{Issue, Provenance, Residual, Solver, Status},
        r#type::{
            Argument, Binder, Binding, BoundRef, Database, DeclId, Rest, Type, TypeId, UnitId,
            UnitSpan,
        },
    },
};

/// A check the solver could not decide, so it did not pass
pub(crate) struct Unresolved {
    pub(crate) span: UnitSpan,
    pub(crate) residual: Residual,
}

/// Check the well-formedness of every declaration and written type. Violations
/// are diagnosed; undecided checks are returned.
pub(crate) fn wellformed(
    db: &Database,
    tables: &Tables<'_>,
    diags: &mut Vec<UnitDiag>,
) -> Vec<Unresolved> {
    let mut check = Check {
        db,
        tables,
        diags,
        unresolved: Vec::new(),
        bounds: (tables.sites.iter())
            .filter(|site| matches!(site.role, Role::Bound(_)))
            .map(|site| {
                let span = UnitSpan {
                    unit: site.unit,
                    span: site.ty.span(),
                };
                (span, &site.ty)
            })
            .collect(),
    };
    for site in &tables.sites {
        if matches!(site.role, Role::Pattern) {
            continue;
        }
        let scope = site.group().map(|key| check.declaration(key));
        check.ty(site.unit, scope, &site.ty, false);
    }
    for index in 0..tables.decls.len() {
        let id = DeclId::from_index(index);
        for sig in 0..tables.sig_count(id) {
            check.signature(id, sig);
        }
    }
    check.recursion();
    check.unresolved
}

/// Whether a check held
enum Verdict {
    Holds,
    Fails,
    Undecided(Residual),
}

struct Check<'a, 'u> {
    db: &'a Database,
    tables: &'a Tables<'u>,
    diags: &'a mut Vec<UnitDiag>,
    unresolved: Vec<Unresolved>,
    /// Each binder bound written, by its span
    bounds: HashMap<UnitSpan, &'a TypeExpr>,
}

impl Check<'_, '_> {
    /// The database declaration of a declaration signature
    fn declaration(&self, key: (DeclId, usize)) -> DeclId {
        self.tables.sig_decls[&key]
    }

    /// Relate two types interpreted in `scope`'s group, holding its binders rigid,
    /// with a solver of its own
    fn relate(&self, scope: Option<DeclId>, actual: TypeId, expected: TypeId) -> Verdict {
        let mut solver = Solver::new(self.db);
        #[cfg(feature = "debug")]
        {
            let (db, tables) = (self.db, self.tables);
            solver.named(move |ty| tables.render_type(db, ty));
        }
        let environment = match scope {
            Some(decl) => solver.rigid_environment(decl),
            None => solver.empty_environment(),
        };
        solver.constrain(
            solver.view(actual, environment),
            solver.view(expected, environment),
            Provenance::default(),
        );
        let outcome = solver.solve().remove(0);
        match outcome.status {
            Status::Proven => Verdict::Holds,
            Status::Contradicted => Verdict::Fails,
            Status::Unresolved => Verdict::Undecided(
                outcome
                    .diagnostics
                    .iter()
                    .find_map(|diagnostic| match diagnostic.issue {
                        Issue::Residual(residual) => Some(residual),
                        Issue::Contradiction(_) => None,
                    })
                    .unwrap_or(Residual::Unsupported("no reason recorded")),
            ),
        }
    }

    /// Diagnose a failed check, or record an undecided one
    fn report(&mut self, verdict: Verdict, span: UnitSpan, diag: impl Report + 'static) {
        #[cfg(feature = "debug")]
        if !matches!(verdict, Verdict::Holds) {
            let mut message = String::new();
            let _ = diag.message(&mut message);
            let location = self.tables.locate(span.unit, span.span);
            match verdict {
                Verdict::Undecided(residual) => dolang_util::debug_eprintln!(
                    topic: "typeck.wf",
                    "{location}: undecided ({residual:?}): {message}"
                ),
                _ => {
                    dolang_util::debug_eprintln!(topic: "typeck.wf", "{location}: fails: {message}")
                }
            }
        }
        match verdict {
            Verdict::Holds => {}
            Verdict::Fails => self.diags.push((span.unit, Diag::new(diag))),
            Verdict::Undecided(residual) => self.unresolved.push(Unresolved { span, residual }),
        }
    }

    /// Check a written type and everything written within it
    fn ty(&mut self, unit: UnitId, scope: Option<DeclId>, ty: &TypeExpr, phantom: bool) {
        match ty {
            TypeExpr::Name { .. } | TypeExpr::Const { .. } | TypeExpr::Error { .. } => {}
            TypeExpr::Group { ty, .. } => self.ty(unit, scope, ty, phantom),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    self.ty(unit, scope, member, phantom);
                }
            }
            // The designated classes these forms stand for bound nothing
            TypeExpr::Schema { params, .. }
            | TypeExpr::Tuple { params, .. }
            | TypeExpr::Record { params, .. } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    self.ty(unit, scope, ty, phantom);
                }
            }
            TypeExpr::App { args, .. } => {
                let phantom = self.application(unit, scope, ty.span(), args) || phantom;
                for arg in args {
                    self.ty(unit, scope, arg.ty(), phantom);
                }
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                self.function(unit, scope, ty, phantom);
                for ty in params.iter().flat_map(TypeParam::tys) {
                    self.ty(unit, scope, ty, phantom);
                }
                for ty in [input, output].into_iter().flatten() {
                    self.ty(unit, scope, ty, phantom);
                }
                self.ty(unit, scope, ret, phantom);
            }
        }
    }

    /// Check each argument of the application written at `span` against its
    /// binder's bound. Whether the base is `Phantom`, whose arguments only mark
    /// variance.
    fn application(
        &mut self,
        unit: UnitId,
        scope: Option<DeclId>,
        span: Span,
        written: &[TypeArg],
    ) -> bool {
        let span = UnitSpan { unit, span };
        let Some(&id) = self.tables.expr_types.get(&span) else {
            return false;
        };
        let Type::Apply { base, args, .. } = self.db.ty(id) else {
            return false;
        };
        let Type::Decl(decl) = *self.db.ty(*base) else {
            return false;
        };
        // `Phantom` marks variance with any types or schemas, so its pack has no
        // shape to satisfy
        if self.tables.designated.get(&decl) == Some(&Designated::Phantom) {
            return true;
        }
        // Where arguments after an expansion go is not known
        let Some(args) = args
            .iter()
            .map(|arg| match *arg {
                Argument::Positional(ty) => Some(ty),
                Argument::Keyword(..) | Argument::Expand(_) => None,
            })
            .collect::<Option<Vec<_>>>()
        else {
            self.unresolved.push(Unresolved {
                span,
                residual: Residual::GenericArguments,
            });
            return false;
        };
        let declaration = self.db.declaration(decl);
        let Type::Quantified { binders, .. } = self.db.ty(declaration.ty) else {
            return false;
        };
        let lifted = self.tables.lifted.get(&decl).map_or(0, Vec::len);
        let fills = self.tables.fill(unit, decl, written);
        for (slot, binder) in binders.iter().enumerate() {
            let Some(bound) = self.db.binder_bound(binder, &args) else {
                continue;
            };
            let verdict = self.relate(scope, args[slot], bound);
            // The written arguments filling the slot, or else the whole application
            let at = slot.checked_sub(lifted).and_then(|slot| {
                fills
                    .iter()
                    .zip(written)
                    .filter(|(fill, _)| {
                        matches!(fill, Fill::Binder(s) | Fill::Item(s) | Fill::Expand(Some(s)) if *s == slot)
                    })
                    .map(|(_, arg)| arg.ty().span())
                    .reduce(|first, last| first | last)
            });
            let at = at.map_or(span, |span| UnitSpan { unit, span });
            let binder = self.binder(decl, slot, binder);
            self.report(
                verdict,
                at,
                BoundViolation {
                    span: at.span,
                    binder,
                    default: false,
                },
            );
        }
        false
    }

    /// A binder with its bound as written, such as `T @ Num` or `*Ts @ {*Value}`
    fn binder(&self, decl: DeclId, slot: usize, binder: &Binder) -> String {
        let source = &self.db.declaration(decl).binders[slot];
        let sigil = match binder.binding {
            Binding::Positional | Binding::Implicit => "",
            Binding::Keyword(_) => ":",
            Binding::Rest(Rest::Positional) => "*",
            Binding::Rest(Rest::Keyed) => "**",
            Binding::Rest(Rest::All) => "...",
        };
        let bound = match (source.bound, binder.binding) {
            (Some(bound), _) => self.tables.print(bound.unit, self.bounds[&bound]),
            (None, Binding::Rest(Rest::Positional)) => "{*Value}".to_owned(),
            (None, Binding::Rest(Rest::Keyed)) => "{**Value}".to_owned(),
            (None, _) => "{...}".to_owned(),
        };
        format!("{sigil}{} @ {bound}", self.db.symbol(source.name))
    }

    /// Check a written function type's parameter keys and channels
    fn function(&mut self, unit: UnitId, scope: Option<DeclId>, ty: &TypeExpr, phantom: bool) {
        let span = UnitSpan {
            unit,
            span: ty.span(),
        };
        let Some(&id) = self.tables.expr_types.get(&span) else {
            return;
        };
        let Type::Function(function) = self.db.ty(id) else {
            return;
        };
        if !phantom {
            let verdict = self.relate(scope, function.params, self.db.rest_shape(Rest::All));
            self.report(verdict, span, ParameterKeys(span.span));
        }
    }

    /// Check a declaration signature as declared: its binder defaults, and a def's
    /// or method's parameter keys and written channels
    fn signature(&mut self, id: DeclId, sig: usize) {
        let decl = self.declaration((id, sig));
        if let DeclNode::Class(class) = &self.tables.decls[id.index()].node {
            self.supertypes(id, decl, class);
        }
        let declaration = self.db.declaration(decl);
        let Type::Quantified { binders, body } = self.db.ty(declaration.ty) else {
            return self.function_signature(id, decl, declaration.ty);
        };
        for (slot, binder) in binders.iter().enumerate() {
            let (Some(default), Some(span)) = (binder.default, declaration.binders[slot].default)
            else {
                continue;
            };
            // Both are interpreted in the group, so they relate under its rigids as written
            let bound = match (binder.bound, binder.binding) {
                (Some(bound), _) => bound,
                (None, Binding::Rest(rest)) => self.db.rest_shape(rest),
                (None, _) => continue,
            };
            let verdict = self.relate(Some(decl), default, bound);
            let binder = self.binder(decl, slot, binder);
            self.report(
                verdict,
                span,
                BoundViolation {
                    span: span.span,
                    binder,
                    default: true,
                },
            );
        }
        self.function_signature(id, decl, *body);
    }

    /// Check a class's supertypes, which are applications with no type expression
    /// of their own
    fn supertypes(&mut self, id: DeclId, decl: DeclId, class: &Class) {
        let unit = self.tables.decls[id.index()].unit;
        for super_ref in &class.supers {
            let span = super_ref.span();
            let phantom = self.application(unit, Some(decl), span, &super_ref.args);
            for arg in &super_ref.args {
                self.ty(unit, Some(decl), arg.ty(), phantom);
            }
        }
    }

    fn function_signature(&mut self, id: DeclId, decl: DeclId, body: TypeId) {
        if let DeclNode::Class(_) | DeclNode::Alias(_) = self.tables.decls[id.index()].node {
            return;
        }
        let Type::Function(function) = self.db.ty(body) else {
            return;
        };
        let name = self.db.declaration(decl).source.span;
        let verdict = self.relate(Some(decl), function.params, self.db.rest_shape(Rest::All));
        self.report(verdict, name, ParameterKeys(name.span));
    }

    /// Check recursion among transparent aliases: every cycle must pass through a
    /// reference guarded by a nominal application's arguments, a function type or
    /// a schema's items (contractive), and each reference within a cycle must pass
    /// the referring alias's binders unchanged (regular)
    fn recursion(&mut self) {
        let mut aliases: Vec<DeclId> = self
            .tables
            .aliases
            .iter()
            .filter(|(_, head)| !matches!(head, Head::Error))
            .map(|(&id, _)| id)
            .collect();
        aliases.sort_by_key(|id| id.index());
        let mut edges: HashMap<DeclId, Vec<DeclId>> = HashMap::new();
        for &id in &aliases {
            let decl = &self.tables.decls[id.index()];
            let targets = edges.entry(id).or_default();
            if let Some(body) = alias_body(self.tables, &decl.node) {
                body.names(&mut |head, _| {
                    if let Some(Referent::Decl(target)) = self.tables.referents.get(&UnitSpan {
                        unit: decl.unit,
                        span: head.span,
                    }) && self.tables.aliases.contains_key(target)
                    {
                        targets.push(*target);
                    }
                });
            }
        }
        for component in components(&aliases, &edges) {
            let cyclic = component.len() > 1
                || edges
                    .get(&component[0])
                    .is_some_and(|targets| targets.contains(&component[0]));
            if !cyclic {
                continue;
            }
            let members: HashSet<DeclId> = component.iter().copied().collect();
            let mut found = Vec::new();
            for &id in &component {
                let decl = &self.tables.decls[id.index()];
                if let Some(body) = alias_body(self.tables, &decl.node) {
                    self.guarded(decl.unit, id, &members, body, false, &mut found);
                }
            }
            // A cycle needs one guard, so an unguarded reference is only wrong on a
            // cycle of unguarded references
            let mut bare: HashMap<DeclId, Vec<DeclId>> = HashMap::new();
            for reference in found.iter().filter(|reference| !reference.guarded) {
                bare.entry(reference.alias)
                    .or_default()
                    .push(reference.target);
            }
            let mut unguarded = HashMap::new();
            for bare_component in components(&component, &bare) {
                for &id in &bare_component {
                    unguarded.insert(id, bare_component.clone());
                }
            }
            for reference in found {
                let cyclic = !reference.guarded
                    && (reference.alias == reference.target
                        || unguarded[&reference.alias].contains(&reference.target));
                if cyclic || !reference.regular {
                    let diag = BadRecursion {
                        span: reference.span,
                        alias: reference.name,
                        irregular: !cyclic,
                    };
                    #[cfg(feature = "debug")]
                    {
                        let mut message = String::new();
                        let _ = diag.message(&mut message);
                        let location = self.tables.locate(reference.unit, reference.span);
                        dolang_util::debug_eprintln!(topic: "typeck.wf", "{location}: fails: {message}");
                    }
                    self.diags.push((reference.unit, Diag::new(diag)));
                }
            }
        }
    }

    /// Walk a recursive alias's body for references to its cycle, adding each to
    /// `found`
    fn guarded(
        &self,
        unit: UnitId,
        alias: DeclId,
        members: &HashSet<DeclId>,
        ty: &TypeExpr,
        guarded: bool,
        found: &mut Vec<Recursive>,
    ) {
        match ty {
            TypeExpr::Const { .. } | TypeExpr::Error { .. } => {}
            TypeExpr::Group { ty, .. } => self.guarded(unit, alias, members, ty, guarded, found),
            TypeExpr::Union { members: union, .. } => {
                for member in union {
                    self.guarded(unit, alias, members, member, guarded, found);
                }
            }
            TypeExpr::Name { head, .. } => {
                self.reference(unit, alias, members, head, ty.span(), None, guarded, found);
            }
            TypeExpr::App { base, args, .. } => {
                let TypeExpr::Name { head, .. } = base.ungrouped() else {
                    return;
                };
                let target = self.reference(
                    unit,
                    alias,
                    members,
                    head,
                    ty.span(),
                    Some(ty),
                    guarded,
                    found,
                );
                let nominal = target.is_some_and(|target| {
                    !self.tables.aliases.contains_key(&target)
                        && self.db.declaration(target).source.kind.nominal()
                });
                for arg in args {
                    self.guarded(unit, alias, members, arg.ty(), guarded || nominal, found);
                }
            }
            TypeExpr::Schema { params, .. } => {
                for param in params {
                    // An inclusion's items are the schema's own
                    let guarded =
                        guarded || !matches!(param.kind, Some(TypeParamKind::Include { .. }));
                    for ty in param.tys() {
                        self.guarded(unit, alias, members, ty, guarded, found);
                    }
                }
            }
            // These forms apply a class, which guards them as any nominal type does
            TypeExpr::Tuple { params, .. } | TypeExpr::Record { params, .. } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    self.guarded(unit, alias, members, ty, true, found);
                }
            }
            TypeExpr::Func {
                params,
                input,
                output,
                ret,
                ..
            } => {
                for ty in params.iter().flat_map(TypeParam::tys) {
                    self.guarded(unit, alias, members, ty, true, found);
                }
                for ty in [input, output].into_iter().flatten() {
                    self.guarded(unit, alias, members, ty, true, found);
                }
                self.guarded(unit, alias, members, ret, true, found);
            }
        }
    }

    /// Note a name in a recursive alias's body, applied as `app` if it is, if it
    /// refers to the cycle. Returns the declaration it names.
    #[allow(clippy::too_many_arguments)]
    fn reference(
        &self,
        unit: UnitId,
        alias: DeclId,
        members: &HashSet<DeclId>,
        head: &Name,
        span: Span,
        app: Option<&TypeExpr>,
        guarded: bool,
        found: &mut Vec<Recursive>,
    ) -> Option<DeclId> {
        let Some(&Referent::Decl(target)) = self.tables.referents.get(&UnitSpan {
            unit,
            span: head.span,
        }) else {
            return None;
        };
        if members.contains(&target) {
            found.push(Recursive {
                unit,
                alias,
                target,
                span,
                name: self.tables.name(unit, *head).to_owned(),
                guarded,
                regular: self.regular(unit, alias, target, app),
            });
        }
        Some(target)
    }

    /// Whether a reference to `target` passes the referring alias's binders
    /// unchanged
    fn regular(&self, unit: UnitId, alias: DeclId, target: DeclId, app: Option<&TypeExpr>) -> bool {
        let own = self.tables.groups[&(alias, 0)].len();
        let args: Vec<TypeId> = match app {
            Some(app) => {
                let Some(&id) = self.tables.expr_types.get(&UnitSpan {
                    unit,
                    span: app.span(),
                }) else {
                    return true;
                };
                match self.db.ty(id) {
                    Type::Apply { args, .. } => {
                        let Some(args) = args
                            .iter()
                            .map(|arg| match *arg {
                                Argument::Positional(ty) => Some(ty),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()
                        else {
                            return false;
                        };
                        args
                    }
                    // Recovered, and already diagnosed
                    _ => return true,
                }
            }
            // A bare name passes only the binders the target is lifted over
            None => {
                let group = &self.tables.groups[&(alias, 0)];
                let Some(slots) = self.tables.groups[&(target, 0)]
                    .iter()
                    .map(|binder| group.iter().position(|b| b == binder))
                    .collect::<Option<Vec<_>>>()
                else {
                    return false;
                };
                return slots.len() == own && slots.iter().enumerate().all(|(i, &s)| i == s);
            }
        };
        args.len() == own
            && args.iter().enumerate().all(|(slot, &arg)| {
                matches!(*self.db.ty(arg), Type::Bound { reference, .. }
                    if reference == BoundRef::new(0, slot))
            })
    }
}

/// A reference to an alias of a cycle, in the body of one
struct Recursive {
    unit: UnitId,
    /// The alias it's written in
    alias: DeclId,
    target: DeclId,
    span: Span,
    name: String,
    guarded: bool,
    /// Whether it passes the referring alias's binders unchanged
    regular: bool,
}

/// The written body of a transparent alias
fn alias_body<'t>(tables: &'t Tables<'_>, node: &DeclNode) -> Option<&'t TypeExpr> {
    match node {
        DeclNode::Alias(alias) => alias.body.map(|body| tables.site_ty(body)),
        _ => None,
    }
}

/// The strongly connected components of a graph, by Tarjan's algorithm
fn components(nodes: &[DeclId], edges: &HashMap<DeclId, Vec<DeclId>>) -> Vec<Vec<DeclId>> {
    struct Tarjan<'a> {
        edges: &'a HashMap<DeclId, Vec<DeclId>>,
        index: HashMap<DeclId, usize>,
        low: HashMap<DeclId, usize>,
        stack: Vec<DeclId>,
        on_stack: HashSet<DeclId>,
        components: Vec<Vec<DeclId>>,
    }

    impl Tarjan<'_> {
        fn visit(&mut self, node: DeclId) {
            let index = self.index.len();
            self.index.insert(node, index);
            self.low.insert(node, index);
            self.stack.push(node);
            self.on_stack.insert(node);
            for &next in self.edges.get(&node).into_iter().flatten() {
                if !self.index.contains_key(&next) {
                    self.visit(next);
                    let low = self.low[&node].min(self.low[&next]);
                    self.low.insert(node, low);
                } else if self.on_stack.contains(&next) {
                    let low = self.low[&node].min(self.index[&next]);
                    self.low.insert(node, low);
                }
            }
            if self.low[&node] == index {
                let mut component = Vec::new();
                while let Some(member) = self.stack.pop() {
                    self.on_stack.remove(&member);
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                self.components.push(component);
            }
        }
    }

    let mut tarjan = Tarjan {
        edges,
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on_stack: HashSet::new(),
        components: Vec::new(),
    };
    for &node in nodes {
        if !tarjan.index.contains_key(&node) {
            tarjan.visit(node);
        }
    }
    tarjan.components
}
