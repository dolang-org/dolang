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
//! keys, except within a `Phantom` argument, which only marks variance; and each
//! written ambient channel for reaching `Iter` or `Sink`. A declaration's own
//! signature and binder defaults are checked as declared. Recursion among
//! transparent aliases must be contractive and regular.

use std::collections::{HashMap, HashSet};

use super::{
    BadChannel, BadRecursion, BoundViolation, DeclNode, Designated, Fill, Head, ParameterKeys,
    Referent, Role, Tables, UnitDiag,
};
use crate::{
    ast::{AliasBody, Class, Function, TypeArg, TypeExpr, TypeParam, TypeParamKind, visit::Node},
    source::{self, Diagnose, Span},
    typeck::{
        solver::{Issue, Provenance, Reach, Residual, Solver, Status},
        r#type::{
            Argument, Binder, Binding, BoundRef, Database, DeclId, Intrinsic, Rest, Type, TypeId,
            UnitId, UnitSpan,
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
    };
    for site in &tables.sites {
        if matches!(site.role, Role::Pattern) {
            continue;
        }
        let scope = site.group().map(|key| check.declaration(key));
        check.ty(site.unit, scope, site.ty, false);
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
                    .unwrap_or(Residual::Unsupported),
            ),
        }
    }

    /// Whether a type interpreted in `scope`'s group reaches `target`
    fn reaches(&self, scope: Option<DeclId>, ty: TypeId, target: DeclId) -> Verdict {
        let mut solver = Solver::new(self.db);
        let environment = match scope {
            Some(decl) => solver.rigid_environment(decl),
            None => solver.empty_environment(),
        };
        match solver.reach(solver.view(ty, environment), target) {
            Ok(Reach::Reached(_) | Reach::Dynamic) => Verdict::Holds,
            Ok(Reach::Unreached) | Err(Issue::Contradiction(_)) => Verdict::Fails,
            Err(Issue::Residual(residual)) => Verdict::Undecided(residual),
        }
    }

    /// Diagnose a failed check, or record an undecided one
    fn report(&mut self, verdict: Verdict, span: UnitSpan, diag: impl Diagnose + 'static) {
        match verdict {
            Verdict::Holds => {}
            Verdict::Fails => self.diags.push((span.unit, source::Diag::new(diag))),
            Verdict::Undecided(residual) => self.unresolved.push(Unresolved { span, residual }),
        }
    }

    /// Check a written type and everything written within it
    fn ty(&mut self, unit: UnitId, scope: Option<DeclId>, ty: &TypeExpr, phantom: bool) {
        match ty {
            TypeExpr::Name { .. } | TypeExpr::Const { .. } | TypeExpr::Error => {}
            TypeExpr::Group { ty, .. } => self.ty(unit, scope, ty, phantom),
            TypeExpr::Union { members, .. } => {
                for member in members {
                    self.ty(unit, scope, member, phantom);
                }
            }
            TypeExpr::Schema { params, .. } => {
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
                for implicit in [input, output].into_iter().flatten() {
                    self.ty(unit, scope, &implicit.ty, phantom);
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
            (Some(bound), _) => self.tables.text(bound.unit, bound.span),
            (None, Binding::Rest(Rest::Positional)) => "{*Value}",
            (None, Binding::Rest(Rest::Keyed)) => "{**Value}",
            (None, _) => "{...}",
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
        let TypeExpr::Func { input, output, .. } = ty else {
            unreachable!()
        };
        for (written, channel, output) in [
            (input, function.input, false),
            (output, function.output, true),
        ] {
            if let (Some(written), Some(channel)) = (written, channel) {
                self.channel(
                    scope,
                    channel,
                    output,
                    UnitSpan {
                        unit,
                        span: written.ty.span(),
                    },
                );
            }
        }
    }

    /// Check that a written channel reaches `Iter` or `Sink`, when designated
    fn channel(&mut self, scope: Option<DeclId>, ty: TypeId, output: bool, span: UnitSpan) {
        let intrinsic = match output {
            false => Intrinsic::Iter,
            true => Intrinsic::Sink,
        };
        let Some(target) = self.db.intrinsic(intrinsic) else {
            return;
        };
        let Type::Decl(target) = *self.db.ty(target) else {
            return;
        };
        let verdict = self.reaches(scope, ty, target);
        self.report(
            verdict,
            span,
            BadChannel {
                span: span.span,
                output,
            },
        );
    }

    /// Check a declaration signature as declared: its binder defaults, and a def's
    /// or method's parameter keys and written channels
    fn signature(&mut self, id: DeclId, sig: usize) {
        let decl = self.declaration((id, sig));
        if let DeclNode::Class(class) = self.tables.decls[id.index()].node {
            self.supertypes(id, decl, class);
        }
        let declaration = self.db.declaration(decl);
        let Type::Quantified { binders, body } = self.db.ty(declaration.ty) else {
            return self.function_signature(id, sig, decl, declaration.ty);
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
        self.function_signature(id, sig, decl, *body);
    }

    /// Check a class's supertypes, which are applications with no type expression
    /// of their own
    fn supertypes(&mut self, id: DeclId, decl: DeclId, class: &Class) {
        let unit = self.tables.decls[id.index()].unit;
        for super_ref in &class.super_refs {
            let head = super_ref.ident.span;
            let span = super_ref.fields.last().map_or(head, |field| head | field);
            let phantom = self.application(unit, Some(decl), span, &super_ref.args);
            for arg in &super_ref.args {
                self.ty(unit, Some(decl), arg.ty(), phantom);
            }
        }
    }

    fn function_signature(&mut self, id: DeclId, sig: usize, decl: DeclId, body: TypeId) {
        let func: &Function = match &self.tables.decls[id.index()].node {
            DeclNode::Defs(defs) => &defs[sig].func,
            DeclNode::Methods(methods) => &methods[sig].func,
            DeclNode::Closure(func) => func,
            DeclNode::Class(_) | DeclNode::Alias(_) => return,
        };
        let Type::Function(function) = self.db.ty(body) else {
            return;
        };
        let unit = self.tables.decls[id.index()].unit;
        let name = self.db.declaration(decl).source.span;
        let verdict = self.relate(Some(decl), function.params, self.db.rest_shape(Rest::All));
        self.report(verdict, name, ParameterKeys(name.span));
        for (written, channel, output) in [
            (&func.input, function.input, false),
            (&func.output, function.output, true),
        ] {
            if let (Some(written), Some(channel)) = (written, channel) {
                self.channel(
                    Some(decl),
                    channel,
                    output,
                    UnitSpan {
                        unit,
                        span: written.ty.span(),
                    },
                );
            }
        }
    }

    /// Check recursion among transparent aliases: within a cycle, each reference to
    /// one of its aliases must be guarded by a nominal application's arguments, a
    /// function type or a schema's items (contractive), and must pass the
    /// referring alias's binders unchanged (regular)
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
            if let Some(body) = alias_body(&decl.node) {
                body.names(&mut |head, _, _| {
                    if let Some(Referent::Decl(target)) = self.tables.referents.get(&UnitSpan {
                        unit: decl.unit,
                        span: head,
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
            for &id in &component {
                let decl = &self.tables.decls[id.index()];
                if let Some(body) = alias_body(&decl.node) {
                    self.guarded(decl.unit, id, &members, body, false);
                }
            }
        }
    }

    /// Walk a recursive alias's body for references to its cycle
    fn guarded(
        &mut self,
        unit: UnitId,
        alias: DeclId,
        members: &HashSet<DeclId>,
        ty: &TypeExpr,
        guarded: bool,
    ) {
        match ty {
            TypeExpr::Const { .. } | TypeExpr::Error => {}
            TypeExpr::Group { ty, .. } => self.guarded(unit, alias, members, ty, guarded),
            TypeExpr::Union { members: union, .. } => {
                for member in union {
                    self.guarded(unit, alias, members, member, guarded);
                }
            }
            TypeExpr::Name { head, .. } => {
                self.reference(unit, alias, members, *head, ty.span(), None, guarded);
            }
            TypeExpr::App { base, args, .. } => {
                let mut base = &**base;
                while let TypeExpr::Group { ty, .. } = base {
                    base = ty;
                }
                let TypeExpr::Name { head, .. } = base else {
                    return;
                };
                let target =
                    self.reference(unit, alias, members, *head, ty.span(), Some(ty), guarded);
                let nominal = target.is_some_and(|target| {
                    !self.tables.aliases.contains_key(&target)
                        && self.db.declaration(target).source.kind.nominal()
                });
                for arg in args {
                    self.guarded(unit, alias, members, arg.ty(), guarded || nominal);
                }
            }
            TypeExpr::Schema { params, .. } => {
                for param in params {
                    // An inclusion's items are the schema's own
                    let guarded =
                        guarded || !matches!(param.kind, Some(TypeParamKind::Include { .. }));
                    for ty in param.tys() {
                        self.guarded(unit, alias, members, ty, guarded);
                    }
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
                    self.guarded(unit, alias, members, ty, true);
                }
                for implicit in [input, output].into_iter().flatten() {
                    self.guarded(unit, alias, members, &implicit.ty, true);
                }
                self.guarded(unit, alias, members, ret, true);
            }
        }
    }

    /// Check a name in a recursive alias's body, applied as `app` if it is. Returns
    /// the declaration it names.
    #[allow(clippy::too_many_arguments)]
    fn reference(
        &mut self,
        unit: UnitId,
        alias: DeclId,
        members: &HashSet<DeclId>,
        head: Span,
        span: Span,
        app: Option<&TypeExpr>,
        guarded: bool,
    ) -> Option<DeclId> {
        let Some(&Referent::Decl(target)) =
            self.tables.referents.get(&UnitSpan { unit, span: head })
        else {
            return None;
        };
        if !members.contains(&target) {
            return Some(target);
        }
        let name = self.tables.text(unit, head).to_owned();
        let irregular = match guarded {
            false => false,
            true if self.regular(unit, alias, target, app) => return Some(target),
            true => true,
        };
        self.diags.push((
            unit,
            source::Diag::new(BadRecursion {
                span,
                alias: name,
                irregular,
            }),
        ));
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

/// The written body of a transparent alias
fn alias_body<'u>(node: &DeclNode<'u>) -> Option<&'u TypeExpr> {
    match node {
        DeclNode::Alias(alias) => match &alias.body {
            AliasBody::Type(ty) => Some(ty),
            AliasBody::Opaque(_) => None,
        },
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
