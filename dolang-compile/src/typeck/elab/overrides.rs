//! Overrides and protocol conformance.
//!
//! Each class and protocol must conform to each supertype it names: every public
//! member the supertype has must be matched by a compatible member of its own,
//! overriding or inherited, and a class must provide each member a protocol
//! declares. The solver states what each member requires (see
//! `solver/conform.rs`); this pass relates the requirements under the
//! declaration's rigids and reports them. A class must also inherit at runtime
//! each class that a supertype it only claims names, with compatible arguments,
//! as a protocol's supertypes are claims too. A generic class reached more than
//! once in its ancestry must be reached first as an instance below every other.
//!
//! Every check is local: it assumes only the declaration's own bounds, and no
//! verdict is cached. Each is solved on its own fork of a solver holding the
//! declaration's rigids, with its own budget, so a class with many members
//! doesn't exhaust one budget for all of them. A check the solver can't decide is
//! returned as unresolved.

use rustc_hash::FxHashMap;

use super::{
    Decl, DeclNode, Designated, Diag, Hidden, IncoherentClasses, Nonconforming, Scoped, Tables,
    TrivialProtocol, Uncallable, UnitDiag, Unresolved, sig,
    surface::{Class, Member, MemberScope, Param, ParamKind, SiteId},
};
use crate::{
    RestKind,
    ast::SpecialMethod,
    diag::{INVALID_SPAN, Span},
    typeck::{
        solver::{
            Contradicted, Contradiction, Found, FoundKind, Gas, Inheritance, Issue, Lookup, Masked,
            Node, Provenance, Reach, Requirement, RequirementKind, Residual, Solver, Status, Step,
            Term,
        },
        r#type::{
            Argument, Database, DeclId, DeclKind, Element, Intrinsic, Kind, Literal,
            Member as Declared, MemberKey, Multiplicity, NoAncestor, Scope, Style, Type, TypeId,
            UnitId, UnitSpan,
        },
    },
};

/// Check that every class and protocol of `unit` conforms to its supertypes.
/// Violations are diagnosed; undecided checks are returned.
pub(crate) fn overrides(
    db: &Database,
    tables: &Tables<'_>,
    unit: UnitId,
    diags: &mut Vec<UnitDiag>,
) -> Vec<Unresolved> {
    let mut unresolved = Vec::new();
    for id in tables.unit_decl_ids(unit) {
        let decl = tables.decl(id);
        let DeclNode::Class(class) = &decl.node else {
            continue;
        };
        let mut check = Check {
            db,
            tables,
            id,
            unit: decl.unit,
            class,
            diags: &mut *diags,
            unresolved: &mut unresolved,
        };
        check.run();
    }
    unresolved
}

struct Check<'a, 'u> {
    db: &'a Database,
    tables: &'a Tables<'u>,
    id: DeclId,
    unit: UnitId,
    class: &'a Class,
    diags: &'a mut Vec<UnitDiag>,
    unresolved: &'a mut Vec<Unresolved>,
}

/// A check whose judgments are to be solved together and reported
struct Pending {
    span: Span,
    /// The judgments, each actual below its expected, all of which must hold.
    /// A judgment is reversed when it relates the required type below the
    /// provided one.
    pairs: Vec<(Term, Term, bool)>,
    message: String,
    /// The members a requirement matches, whose notes say what of them fails
    matched: Option<Matched>,
}

/// A provided member and the member it must match, as a diagnostic names them
struct Matched {
    /// The provided member, as the headline names it
    subject: String,
    /// The required member, qualified by its class
    required: String,
    /// Each member's methods, which name its parameters and binders
    provided_methods: Vec<Accessor>,
    required_methods: Vec<Accessor>,
    /// Whether each judgment relates the provided member to another of the
    /// required member's overloads
    overloads: bool,
}

/// What of a member fails to match, and where the provided and required members
/// declare that part of them, in whichever units
#[derive(PartialEq)]
struct Explained {
    note: String,
    provided: Option<UnitSpan>,
    required: Option<UnitSpan>,
}

/// A member's method, as its role names what of it a note is about
#[derive(Clone, Copy)]
struct Accessor {
    decl: DeclId,
    /// The method's role, if a property's accessor: "getter's" or "setter's"
    role: Option<&'static str>,
}

impl Check<'_, '_> {
    fn run(&mut self) {
        // A type object's members are its class's, so it is called through the
        // class's `(init)` or class-level `(call)`, which the solver relates to a
        // function type directly
        let designated = self.tables.designated.get(&self.id);
        if designated == Some(&Designated::Intrinsic(Intrinsic::Type)) {
            return;
        }
        let db = self.db;
        let runtime = db.declaration(self.id).source.kind == DeclKind::Class;
        let gas = Gas::default();
        let mut solver = Solver::new(db, &gas);
        #[cfg(feature = "debug")]
        {
            solver.named(self.tables, crate::typeck::r#type::Style::Full);
        }
        let environment = solver.rigid_environment(self.id);
        solver.close();
        let instance = self.instance();
        self.coherent();
        self.trivial();
        self.callable(&solver, instance);
        let spans = self.member_spans();
        if runtime {
            self.hidden(&solver, instance, &spans);
        }
        let mut pending = Vec::new();
        for super_ref in &self.class.supers {
            let span = super_ref.head.span;
            let Some(ty) = super_ref.interned.get() else {
                continue;
            };
            let supertype = solver.view(ty, environment);
            let requirements = match solver.conformance(instance, supertype, runtime, false) {
                Ok(requirements) => requirements,
                Err(issue) => {
                    self.undecided(span, issue);
                    continue;
                }
            };
            for requirement in requirements {
                // A claimed class's member, as the class inherits it, agrees with
                // itself unless the class is inherited with other arguments, which
                // is reported below
                let claimed = runtime
                    && super_ref.type_only
                    && requirement.provider == Some(requirement.required)
                    && db.declaration(requirement.required).source.kind == DeclKind::Class;
                if !claimed {
                    self.requirement(&mut pending, &spans, span, requirement);
                }
            }
            // A claim doesn't make the runtime inherit what it names
            if runtime && super_ref.type_only {
                let name = &self.tables.quote(self.unit, super_ref.head);
                match solver.claimed_classes(solver.closed(instance), supertype) {
                    Ok(classes) => {
                        for (class, inheritance) in classes {
                            self.inheritance(&mut pending, span, name, class, inheritance);
                        }
                    }
                    Err(issue) => self.undecided(span, issue),
                }
            }
        }
        match solver.masked(solver.closed(instance), runtime) {
            Ok(masked) => {
                for masked in masked {
                    self.masked(&solver, &mut pending, masked);
                }
            }
            Err(issue) => {
                if let Some(span) = self.tables.decl(self.id).name_span() {
                    self.undecided(span, issue);
                }
            }
        }
        if designated != Some(&Designated::Value) {
            self.value(&solver, instance, runtime, &spans, &mut pending);
        }
        for check in pending {
            // Each check has a budget of its own, so a class with many members
            // doesn't exhaust one
            let gas = Gas::default();
            let mut fork = solver.fork(&gas);
            // Each constraint's judgment, by whether it's reversed and its index
            let judgments: FxHashMap<_, _> = check
                .pairs
                .into_iter()
                .enumerate()
                .map(|(index, (actual, expected, reversed))| {
                    let constraint = fork.constrain(actual, expected, Provenance::default());
                    (constraint, (reversed, index))
                })
                .collect();
            let outcomes = fork.solve();
            if outcomes.iter().any(|o| o.status == Status::Contradicted) {
                let mut explained = Vec::new();
                if let Some(matched) = &check.matched {
                    let contradicted = outcomes
                        .iter()
                        .filter(|outcome| outcome.status == Status::Contradicted);
                    for outcome in contradicted {
                        let (reversed, index) = judgments[&outcome.constraint];
                        // The judgment of one of several overloads names it
                        let (required, overload) = match matched.overloads {
                            true => (
                                format!("overload {} of `{}`", index + 1, matched.required),
                                Some(index),
                            ),
                            false => (format!("`{}`", matched.required), None),
                        };
                        for cause in fork.explain(outcome).iter() {
                            if let Some(cause) =
                                self.cause(matched, &required, overload, cause, reversed)
                                && !explained.iter().any(|(other, _)| *other == cause)
                            {
                                explained.push((cause, required.clone()));
                            }
                        }
                    }
                }
                // Each note is at the part of the provided member it's about,
                // or else at the required member's, if in this unit, and each
                // member's part elsewhere is pointed to beside it. A part in
                // another unit has only a pointer, so the note is the same
                // whether that unit is checked from source or its typelib.
                let (mut labels, mut notes) = (Vec::new(), Vec::new());
                let subject = (check.matched.as_ref()).map_or("", |matched| &matched.subject);
                for (cause, required) in explained {
                    let here = |span: &Option<UnitSpan>| span.filter(|span| span.unit == self.unit);
                    let at = here(&cause.provided).or(here(&cause.required));
                    let mut placed = Vec::new();
                    match at {
                        Some(at) => placed.push((at, cause.note)),
                        None => notes.push(cause.note),
                    }
                    let beside = [
                        (cause.provided, subject),
                        (cause.required, required.as_str()),
                    ];
                    for (span, member) in beside {
                        if let Some(span) = span
                            && Some(span) != at
                        {
                            placed.push((span, format!("as {member} declares it")));
                        }
                    }
                    for label in placed {
                        if !labels.contains(&label) {
                            labels.push(label);
                        }
                    }
                }
                self.nonconforming(check.span, check.message, labels, notes);
            } else if let Some(outcome) = outcomes.iter().find(|o| o.status == Status::Unresolved) {
                // A form the checker doesn't support leaves what depends on it
                // unsolved, so it's the reason
                let residuals =
                    outcome
                        .diagnostics
                        .iter()
                        .filter_map(|diagnostic| match diagnostic.issue {
                            Issue::Residual(residual) => Some(residual),
                            Issue::Contradiction(_) => None,
                        });
                let residual = residuals
                    .clone()
                    .find(|residual| matches!(residual, Residual::Unsupported(_)))
                    .or_else(|| residuals.clone().next())
                    .unwrap_or(Residual::Unsupported("no reason recorded"));
                self.unresolved.push(Unresolved {
                    span: UnitSpan {
                        unit: self.unit,
                        span: check.span,
                    },
                    residual,
                });
            }
        }
    }

    /// Report a class inheriting from two unrelated disjoint classes, which
    /// leaves it no place among them
    fn coherent(&mut self) {
        let Err(NoAncestor::Incoherent(first, second)) = self.db.supertypes_ancestor(self.id)
        else {
            return;
        };
        let Some(span) = self.tables.decl(self.id).name_span() else {
            return;
        };
        let name = |decl: Option<DeclId>| decl.map_or("Value", |decl| self.name(decl)).to_owned();
        let diag = IncoherentClasses {
            span,
            class: self.name(self.id).to_owned(),
            first: name(first),
            second: name(second),
        };
        self.diags.push((self.unit, Diag::new(diag)));
    }

    /// Warn of an instance `(call)` in a class that doesn't reach `Func`, since
    /// the checker won't pass its instances as functions.
    fn callable(&mut self, solver: &Solver<'_>, instance: TypeId) {
        let (db, tables) = (self.db, self.tables);
        let call = self.class.members.iter().find_map(|member| {
            let Member::Method { decl, sig } = *member else {
                return None;
            };
            let method = tables.method(decl, sig);
            let instance = sig::method_scope(method) == Scope::Instance;
            (instance && matches!(method.special, Some(SpecialMethod::Call)))
                .then_some(method.name.span)
        });
        let Some(span) = call else {
            return;
        };
        let Some(&Type::Decl(func)) = db.intrinsic(Intrinsic::Func).map(|func| db.ty(func)) else {
            return;
        };
        if let Ok(Reach::Unreached) = solver.reach(solver.closed(instance), func) {
            let class = self.name(self.id).to_owned();
            self.diags
                .push((self.unit, Diag::new(Uncallable { span, class })));
        }
    }

    /// Warn of a protocol declaring required members, all of which `Value`
    /// declares. One that declares none is a marker, and one only inheriting
    /// such members is warned of where they're declared.
    fn trivial(&mut self) {
        let db = self.db;
        let declares = db
            .declaration(self.id)
            .members
            .iter()
            .any(|(key, member)| db.required_member(*key, member));
        if !declares || !db.trivial_protocol(self.id) {
            return;
        }
        let Some(span) = self.tables.decl(self.id).name_span() else {
            return;
        };
        let protocol = self.name(self.id).to_owned();
        self.diags
            .push((self.unit, Diag::new(TrivialProtocol { span, protocol })));
    }

    /// Warn of an instance method that hides a class or static member of the same
    /// name from the class object, which reaches the method first. Each pair is
    /// reported where the later of the two is declared: at an own method, or at
    /// an own class member that an inherited method hides. Special methods are
    /// exempt, as a class `(call)` is reached by calling the class.
    fn hidden(
        &mut self,
        solver: &Solver<'_>,
        instance: TypeId,
        spans: &FxHashMap<(MemberKey, bool), Span>,
    ) {
        let db = self.db;
        let receiver = solver.closed(instance);
        // A private member is named by its class alone, so only the class's own
        // members share its key
        let own = |key: MemberKey, admits: &dyn Fn(&Declared) -> bool| {
            db.declaration(self.id)
                .members
                .iter()
                .find(|(found, member)| *found == key && admits(member))
                .map(|(_, member)| (self.id, member.scope()))
        };
        let inherited = |key: MemberKey, scope: Scope| -> Option<Found> {
            match solver.inherited_member(receiver, key, scope, true) {
                Ok(Lookup::Found(found)) => Some(found),
                _ => None,
            }
        };
        let mut hidden = Vec::new();
        for (key, member) in db.declaration(self.id).members.iter() {
            if key.special {
                continue;
            }
            if member.scope() == Scope::Instance {
                if !matches!(member, Declared::Method { .. }) {
                    continue;
                }
                let class_member = match key.private {
                    true => own(*key, &|member| member.scope() != Scope::Instance),
                    false => [Scope::Class, Scope::Static]
                        .into_iter()
                        .find_map(|scope| inherited(*key, scope))
                        .map(|found| (found.class, found.scope)),
                };
                if let Some((owner, scope)) = class_member
                    && let Some(&span) = spans.get(&(*key, true))
                {
                    hidden.push((span, *key, self.id, owner, scope));
                }
            } else if !key.private
                && let Some(found) = inherited(*key, Scope::Instance)
                && found.class != self.id
                && matches!(found.kind, FoundKind::Method(_))
                && let Some(&span) = spans.get(&(*key, false))
            {
                hidden.push((span, *key, found.class, self.id, member.scope()));
            }
        }
        let class = self.name(self.id).to_owned();
        for (span, key, method, owner, scope) in hidden {
            let name = self.db.member_name(key);
            let diag = Hidden {
                span,
                method: format!("{}.{name}", self.name(method)),
                member: format!("{}.{name}", self.name(owner)),
                scope,
                class: class.clone(),
            };
            self.diags.push((self.unit, Diag::new(diag)));
        }
    }

    /// Relate the members the declaration declares itself to `Value`'s, which it
    /// has without naming it. One it inherits was related where it's declared.
    fn value(
        &mut self,
        solver: &Solver<'_>,
        instance: TypeId,
        runtime: bool,
        spans: &FxHashMap<(MemberKey, bool), Span>,
        pending: &mut Vec<Pending>,
    ) {
        let db = self.db;
        if db.intrinsic(Intrinsic::Value).is_none() {
            return;
        }
        let Some(name) = self.tables.decl(self.id).name else {
            return;
        };
        match solver.conformance(instance, solver.closed(db.top()), runtime, false) {
            Ok(requirements) => {
                for requirement in requirements {
                    if requirement.provider == Some(self.id) {
                        self.requirement(pending, spans, name.span, requirement);
                    }
                }
            }
            Err(issue) => self.undecided(name.span, issue),
        }
    }

    /// Relate or report what a supertype's member requires
    fn requirement(
        &mut self,
        pending: &mut Vec<Pending>,
        spans: &FxHashMap<(MemberKey, bool), Span>,
        supertype: Span,
        requirement: Requirement,
    ) {
        let member = self.db.member_name(requirement.key);
        let required = format!("{}.{member}", self.name(requirement.required));
        // An override is reported where it's declared
        let declared = requirement.provider == Some(self.id);
        let span = match declared {
            true => spans
                .get(&(requirement.key, requirement.scope == Scope::Instance))
                .copied()
                .unwrap_or(supertype),
            false => supertype,
        };
        let subject = match (declared, requirement.provider) {
            (false, Some(provider)) => format!("`{member}` from `{}`", self.name(provider)),
            _ => format!("`{member}`"),
        };
        match requirement.kind {
            RequirementKind::Relate(pairs) => {
                let (key, scope) = (requirement.key, requirement.scope);
                let provided = requirement
                    .provider
                    .map(|provider| self.methods(provider, key, scope));
                let (provided_methods, provided_method) = provided.unwrap_or_default();
                let (required_methods, required_method) =
                    self.methods(requirement.required, key, scope);
                let matched = Matched {
                    subject: subject.clone(),
                    required: required.clone(),
                    provided_methods,
                    required_methods,
                    overloads: provided_method && required_method && pairs.len() > 1,
                };
                pending.push(Pending {
                    span,
                    pairs,
                    message: format!("{subject} does not match `{required}`"),
                    matched: Some(matched),
                });
            }
            RequirementKind::Missing => {
                let message = format!(
                    "`{}` does not provide `{member}`, which `{}` requires",
                    self.name(self.id),
                    self.name(requirement.required)
                );
                self.report(span, message);
            }
            RequirementKind::Changed {
                provided,
                required: kind,
            } => {
                let message = format!("{subject} is {provided}, but `{required}` is {kind}");
                self.report(span, message);
            }
            RequirementKind::Undecided(issue) => self.undecided(span, issue),
        }
    }

    /// Relate or report how a class inherits a class its claim names
    fn inheritance(
        &mut self,
        pending: &mut Vec<Pending>,
        span: Span,
        claim: &str,
        class: DeclId,
        inheritance: Inheritance,
    ) {
        let own = self.name(self.id);
        let named = self.name(class);
        match inheritance {
            Inheritance::Unreached => {
                let message =
                    format!("`{claim}` requires `{named}`, which `{own}` does not inherit");
                self.report(span, message);
            }
            Inheritance::Relate(actual, expected) => {
                pending.push(Pending {
                    span,
                    pairs: vec![(actual, expected, false)],
                    message: format!(
                        "`{own}` inherits `{named}` with arguments `{claim}` does not allow"
                    ),
                    matched: None,
                });
            }
            Inheritance::Undecided(issue) => self.undecided(span, issue),
        }
    }

    /// Relate the first instance of a generic class in the ancestry to one it
    /// masks, reporting at the supertype that brings the first
    fn masked(&mut self, solver: &Solver<'_>, pending: &mut Vec<Pending>, masked: Masked) {
        let Some((span, through)) = self.supertype(masked.first.supertype) else {
            return;
        };
        let Some((_, other_through)) = self.supertype(masked.other.supertype) else {
            return;
        };
        let names = Scoped {
            tables: self.tables,
            scope: vec![self.id],
        };
        let render = |ty: TypeId, direct: bool, through: &str| {
            let ty = self.db.render(ty, &names, Style::Reader);
            match direct {
                true => format!("`{ty}`"),
                false => format!("`{ty}` from `{through}`"),
            }
        };
        let mut message = format!(
            "{} is not below {}",
            render(masked.first.ty, masked.first.direct, &through),
            render(masked.other.ty, masked.other.direct, &other_through),
        );
        if masked.reorder {
            message.push_str(&format!("; name `{other_through}` before `{through}`"));
        }
        // The class's own rule relates the instances' arguments by its variances
        let pairs = vec![(
            solver.closed(masked.first.ty),
            solver.closed(masked.other.ty),
            false,
        )];
        pending.push(Pending {
            span,
            pairs,
            message,
            matched: None,
        });
    }

    /// Where the declaration names its supertype at `index`, and its name
    fn supertype(&self, index: usize) -> Option<(Span, String)> {
        let ty = self.db.declaration(self.id).supertypes.get(index)?.ty;
        self.class.supers.iter().find_map(|super_ref| {
            let span = super_ref.head.span;
            (super_ref.interned.get() == Some(ty)).then(|| {
                let name = self.tables.quote(self.unit, super_ref.head);
                (span, name)
            })
        })
    }

    /// The declaration's own type, its class applied to its rigids
    fn instance(&self) -> TypeId {
        let db = self.db;
        let decl = db.intern(Type::Decl(self.id));
        let rigids = db.rigids(self.id);
        if rigids.is_empty() {
            return decl;
        }
        db.intern(Type::Apply {
            base: decl,
            args: rigids
                .into_iter()
                .map(Argument::Positional)
                .collect::<Vec<_>>()
                .into(),
            kind: Kind::Type,
        })
    }

    /// Where each member the class declares is named, by its name and whether it
    /// is an instance member
    fn member_spans(&self) -> FxHashMap<(MemberKey, bool), Span> {
        let (db, tables, unit) = (self.db, self.tables, self.unit);
        let mut spans = FxHashMap::default();
        for member in &self.class.members {
            match *member {
                Member::Field(ref field) => {
                    for &name in &field.names {
                        let key = MemberKey {
                            name: db.intern_symbol(tables.name(unit, name)),
                            special: false,
                            private: !field.public,
                        };
                        spans
                            .entry((key, field.scope == MemberScope::Instance))
                            .or_insert(name.span);
                    }
                }
                Member::Method { decl, sig } => {
                    let method = tables.method(decl, sig);
                    let key = MemberKey {
                        name: db.intern_symbol(tables.name(unit, method.name)),
                        special: method.special.is_some(),
                        private: !method.public && method.special.is_none(),
                    };
                    let instance = sig::method_scope(method) == Scope::Instance;
                    spans.entry((key, instance)).or_insert(method.name.span);
                }
            }
        }
        spans
    }

    /// The methods declaring `class`'s member `key` of `scope`: a method, or
    /// a property's setter and getter. It's a method if the flag is set.
    fn methods(&self, class: DeclId, key: MemberKey, scope: Scope) -> (Vec<Accessor>, bool) {
        let members = &self.db.declaration(class).members;
        let declared = members.iter().find_map(|(member, declared)| {
            let of = match *declared {
                Declared::Field { scope, .. }
                | Declared::Method { scope, .. }
                | Declared::Property { scope, .. }
                | Declared::Decorated { scope, .. } => scope,
            };
            (*member == key && of == scope).then_some(declared)
        });
        match declared {
            Some(&Declared::Method { decl, .. }) => (vec![Accessor { decl, role: None }], true),
            Some(&Declared::Property { getter, setter, .. }) => {
                let setter = setter.map(|decl| Accessor {
                    decl,
                    role: Some("setter's"),
                });
                let getter = getter.map(|decl| Accessor {
                    decl,
                    role: Some("getter's"),
                });
                (setter.into_iter().chain(getter).collect(), false)
            }
            _ => (Vec::new(), false),
        }
    }

    /// What of a member fails to match, by the contradiction's path: the
    /// first part of the member's type the path enters, with the provided and
    /// required members' types of it, or the item one has that the other can't
    /// take. The path's root relates the required member's type below the
    /// provided one's if `reversed`. The judgment is of the required member's
    /// `overload`, if it has several.
    fn cause(
        &self,
        matched: &Matched,
        required: &str,
        overload: Option<usize>,
        cause: &Contradicted,
        reversed: bool,
    ) -> Option<Explained> {
        let names = Scoped {
            tables: self.tables,
            scope: vec![self.id],
        };
        let path = &cause.path;
        // Each type as the provided member's and the required member's, by
        // whether a node's actual side is the required member's
        let pair = |sides: (TypeId, TypeId), flipped: bool| {
            let (provided, required) = match flipped {
                false => sides,
                true => (sides.1, sides.0),
            };
            self.db.pair(provided, required, &names, Style::Reader)
        };
        let both = |node: &Node| match node.sides {
            Some((Some(actual), Some(expected))) => Some((actual, expected)),
            _ => None,
        };
        // Where each member declares a part of its signature, found by `find`
        // in the first of its methods `getter` admits: a property's getter, if
        // set, or any method
        let spans = |getter: bool, find: &dyn Fn(&Decl<'_>, usize) -> Option<Span>| {
            let side = |methods: &[Accessor], overload| {
                let mut admitted = methods
                    .iter()
                    .filter(|method| !getter || matches!(method.role, None | Some("getter's")));
                admitted.find_map(|method| {
                    let (decl, sig) = self.local(method.decl, overload)?;
                    let span = find(decl, sig)?;
                    Some(UnitSpan {
                        unit: decl.unit,
                        span,
                    })
                })
            };
            (
                side(&matched.provided_methods, None),
                side(&matched.required_methods, overload),
            )
        };
        let mut flipped = reversed;
        // Members a protocol judged structurally requires, outermost first
        let mut within = Vec::new();
        let component = |within: &[String], label: String| match within.is_empty() {
            true => label,
            false => format!("{}, {label}", within.join(", ")),
        };
        for (index, node) in path.iter().enumerate() {
            let Some(step) = &node.step else {
                continue;
            };
            flipped ^= step.reverses();
            // Within a member a protocol requires, a part is of the member's
            // type, which neither member declares
            let declared = within.is_empty();
            let (label, (provided, required_span)) = match *step {
                Step::Member { key, .. } => {
                    within.push(format!("member `{}`", self.db.member_name(key)));
                    continue;
                }
                Step::Parameters if index + 1 == path.len() => {
                    let mut explained =
                        self.arity(matched, required, node, cause.contradiction, flipped)?;
                    // The item is the list's that has it: the required member's
                    // if passed in excess, else the provided one's
                    let (actual, expected) = both(node)?;
                    let (schema, item, excess) = match cause.contradiction {
                        Contradiction::Excess(item) => (actual, item, true),
                        Contradiction::Missing(item) => (expected, item, false),
                        _ => return Some(explained),
                    };
                    if declared {
                        let (provided, required) = spans(false, &|decl, sig| {
                            let param = self.param(decl, sig, schema, item)?;
                            self.param_name(param)
                        });
                        match excess {
                            true => explained.required = required,
                            false => explained.provided = provided,
                        }
                    }
                    return Some(explained);
                }
                Step::Item(item) | Step::Key(item)
                    if matches!(path[index - 1].step, Some(Step::Parameters)) =>
                {
                    // The item is the actual side's, a positional one at the
                    // same ordinal on either side, so the provided member names
                    // it as the headline's member does
                    let (schema, _) = both(&path[index - 1])?;
                    let methods = matched.provided_methods.iter();
                    let label = self.parameter(
                        methods.chain(&matched.required_methods),
                        schema,
                        item,
                        &names,
                    );
                    let found = spans(false, &|decl, sig| {
                        let param = self.param(decl, sig, schema, item)?;
                        let annot = param.annot.and_then(|annot| self.written(annot));
                        annot.or_else(|| self.param_name(param))
                    });
                    (label, found)
                }
                // A property's value is returned by its getter, as a field's
                // is related to one
                Step::Return => {
                    let methods = matched.provided_methods.iter();
                    let label = match methods
                        .chain(&matched.required_methods)
                        .any(|method| method.role.is_some())
                    {
                        true => "getter's return type".to_owned(),
                        false => "return type".to_owned(),
                    };
                    let found = spans(true, &|decl, sig| {
                        let ret = decl.node.signature(sig).ret;
                        (ret.and_then(|ret| self.written(ret))).or_else(|| decl.name_span())
                    });
                    (label, found)
                }
                Step::Input | Step::Output => {
                    let input = matches!(step, Step::Input);
                    let found = spans(false, &|decl, sig| {
                        let signature = decl.node.signature(sig);
                        let site = match input {
                            true => signature.input,
                            false => signature.output,
                        };
                        (site.and_then(|site| self.written(site))).or_else(|| decl.name_span())
                    });
                    let label = match input {
                        true => "input",
                        false => "output",
                    };
                    (label.to_owned(), found)
                }
                Step::InstantiationBound(binder) => {
                    // A fresh variable instantiates the actual side's binder,
                    // and what it's solved as must be below its bound
                    let (methods, overload) = match flipped {
                        false => (&matched.provided_methods, None),
                        true => (&matched.required_methods, overload),
                    };
                    let method = methods.first().map(|method| method.decl);
                    let (_, Some(bound)) = node.sides? else {
                        return None;
                    };
                    let found = path[index + 1..]
                        .iter()
                        .find_map(|node| node.sides.and_then(|(actual, _)| actual))?;
                    let (bound, found) = (
                        self.db.render(bound, &names, Style::Reader),
                        self.db.render(found, &names, Style::Reader),
                    );
                    let name = method.and_then(|method| self.binder_name(method, binder));
                    let label = match name {
                        Some(name) => format!("binder `{name}`"),
                        None => format!("binder {}", binder + 1),
                    };
                    let label = component(&within, label);
                    // The binder's bound, or else its name, as its member
                    // declares it
                    let span = method
                        .zip(name)
                        .filter(|_| declared)
                        .and_then(|(method, name)| {
                            let (decl, sig) = self.local(method, overload)?;
                            let binders = self.tables.binders(method, sig);
                            let binder = binders
                                .iter()
                                .find(|binder| self.tables.name(decl.unit, binder.name) == name)?;
                            let bound = binder.bound.and_then(|bound| self.written(bound));
                            let span = bound.unwrap_or(binder.name.span);
                            Some(UnitSpan {
                                unit: decl.unit,
                                span,
                            })
                        });
                    let (provided, required_span) = match flipped {
                        false => (span, None),
                        true => (None, span),
                    };
                    return Some(Explained {
                        note: format!(
                            "{label}: bound `{bound}` here, but {required} passes `{found}`"
                        ),
                        provided,
                        required: required_span,
                    });
                }
                _ => continue,
            };
            let (provided_ty, expected) = pair(both(node)?, flipped);
            let label = component(&within, label);
            let (provided, required_span) = match declared {
                true => (provided, required_span),
                false => (None, None),
            };
            return Some(Explained {
                note: format!("{label}: {provided_ty} here, but {expected} in {required}"),
                provided,
                required: required_span,
            });
        }
        // A member that isn't a function, as a field, shows its type whole
        let root = both(path.first()?)?;
        let function =
            |ty: TypeId| matches!(self.db.ty(ty), Type::Function(_) | Type::Quantified { .. });
        if function(root.0) || function(root.1) {
            return None;
        }
        let (provided, expected) = pair(root, reversed);
        Some(Explained {
            note: format!("{provided} here, but {expected} in {required}"),
            provided: None,
            required: None,
        })
    }

    /// A note on a parameter one member's list has and the other's can't take,
    /// where the list is at `node`, flipped if its actual side is the required
    /// member's
    fn arity(
        &self,
        matched: &Matched,
        required: &str,
        node: &Node,
        contradiction: Contradiction,
        flipped: bool,
    ) -> Option<Explained> {
        let names = &Scoped {
            tables: self.tables,
            scope: vec![self.id],
        };
        let (actual, expected) = match node.sides? {
            (Some(actual), Some(expected)) => (actual, expected),
            _ => return None,
        };
        let subject = &matched.subject;
        let item =
            |schema: TypeId, index: usize| self.db.schema_item(schema, index, names, Style::Reader);
        let note = match (contradiction, flipped) {
            // The required member is called with an item the provided one
            // doesn't take
            (Contradiction::Excess(index), true) => {
                let item = item(actual, index)?;
                format!("{required} passes {item}, which {subject} doesn't take")
            }
            // The provided member requires an item the required one may not be
            // called with
            (Contradiction::Missing(index), true) => {
                let item = item(expected, index)?;
                format!("{subject} requires {item}, which {required} may not pass")
            }
            _ => return None,
        };
        Some(Explained {
            note,
            provided: None,
            required: None,
        })
    }

    /// A method's declaration and the signature of it a judgment relates, if
    /// declared in a unit at hand: its `overload`, if it has several, or its only
    /// signature. A method with several overloads has none of them without
    /// `overload`, as the solver chooses among them.
    fn local(&self, method: DeclId, overload: Option<usize>) -> Option<(&Decl<'_>, usize)> {
        let decl = self.tables.try_decl(method)?;
        let sig = match &decl.node {
            DeclNode::Methods(methods) => {
                let mut overloads = methods
                    .iter()
                    .enumerate()
                    .filter(|(_, method)| method.overload)
                    .map(|(sig, _)| sig);
                match (overload, overloads.clone().count()) {
                    (_, 0) => decl.node.primary_sig()?,
                    (Some(overload), _) => overloads.nth(overload)?,
                    (None, 1) => overloads.next()?,
                    (None, _) => return None,
                }
            }
            node => node.primary_sig()?,
        };
        Some((decl, sig))
    }

    /// The parameter of `decl`'s signature `sig` a parameter list's item at
    /// `index` is for
    fn param<'d>(
        &self,
        decl: &'d Decl<'_>,
        sig: usize,
        schema: TypeId,
        index: usize,
    ) -> Option<&'d Param> {
        let Type::Schema(items) = self.db.ty(schema) else {
            return None;
        };
        let item = items.get(index)?;
        let params = &decl.node.signature(sig).params;
        let rest = item.multiplicity == Multiplicity::Repeated;
        let rest_of = |kinds: &[RestKind]| {
            params.iter().find(|param| match param.kind {
                ParamKind::Rest { kind, .. } => kinds.contains(&kind),
                _ => false,
            })
        };
        match (&item.element, rest) {
            (Element::Positional(_), false) => {
                // A positional item's ordinal among its list's
                let ordinal = items[..index]
                    .iter()
                    .filter(|item| matches!(item.element, Element::Positional(_)))
                    .count();
                params
                    .iter()
                    .filter(|param| matches!(param.kind, ParamKind::Pos))
                    .nth(ordinal)
            }
            (Element::Positional(_), true) => rest_of(&[RestKind::Pos, RestKind::Mixed]),
            (Element::Keyed { key, .. }, _) => {
                // A keyed item exactly keyed is its key's parameter
                let named = match (self.db.ty(*key), rest) {
                    (Type::Literal(Literal::Sym(key)), false) => {
                        let key = self.db.symbol(*key);
                        params.iter().find(|param| match param.kind {
                            ParamKind::Key { key: name } => {
                                self.tables.name(decl.unit, name) == key
                            }
                            _ => false,
                        })
                    }
                    _ => None,
                };
                named.or_else(|| rest_of(&[RestKind::Key, RestKind::Mixed]))
            }
            (Element::Include(_), _) => params
                .iter()
                .find(|param| matches!(param.kind, ParamKind::Rest { .. })),
        }
    }

    /// Where a type is written at a site, which a typelib omits
    fn written(&self, site: SiteId) -> Option<Span> {
        let span = self.tables.site(site).ty.span();
        (span != INVALID_SPAN).then_some(span)
    }

    /// Where a parameter is named: by its key, if passed by one
    fn param_name(&self, param: &Param) -> Option<Span> {
        match param.kind {
            ParamKind::Key { key } => Some(key.span),
            _ => param.name.map(|name| name.span),
        }
    }

    /// How a note names a parameter list's item at `index`, by the first of
    /// `methods` that names it
    fn parameter<'m>(
        &self,
        methods: impl IntoIterator<Item = &'m Accessor>,
        schema: TypeId,
        index: usize,
        names: &Scoped<'_, '_>,
    ) -> String {
        let Type::Schema(items) = self.db.ty(schema) else {
            return "a parameter".to_owned();
        };
        let Some(item) = items.get(index) else {
            return "a parameter".to_owned();
        };
        // A keyed item exactly keyed names its parameter
        if let Element::Keyed { key, .. } = item.element
            && let Type::Literal(Literal::Sym(key)) = self.db.ty(key)
        {
            return format!("parameter `{}`", self.db.symbol(*key));
        }
        let declared = methods.into_iter().find_map(|method| {
            let decl = self.tables.try_decl(method.decl)?;
            let sig = decl.node.primary_sig()?;
            let name = self.param(decl, sig, schema, index)?.name?;
            let role = method
                .role
                .map_or_else(String::new, |role| format!("{role} "));
            Some((role, self.tables.name(decl.unit, name).to_owned()))
        });
        let rest = item.multiplicity == Multiplicity::Repeated;
        match (
            declared,
            rest || matches!(item.element, Element::Include(_)),
        ) {
            (Some((role, name)), false) => format!("{role}parameter `{name}`"),
            (Some((role, name)), true) => format!("{role}rest `{name}`"),
            (None, _) => match self.db.schema_item(schema, index, names, Style::Reader) {
                Some(item) => format!("parameter {item}"),
                None => "a parameter".to_owned(),
            },
        }
    }

    /// The name of the binder at `index` of a method's type
    fn binder_name(&self, method: DeclId, index: usize) -> Option<&str> {
        let declaration = self.db.declaration(method);
        let Type::Quantified { binders, .. } = self.db.ty(declaration.ty) else {
            return None;
        };
        // A method's type may leave out binders it has before its own
        let declared = &declaration.binders;
        let lifted = declared.len().checked_sub(binders.len())?;
        Some(self.db.symbol(declared.get(lifted + index)?.name))
    }

    fn name(&self, decl: DeclId) -> &str {
        self.db
            .declaration(decl)
            .source
            .name
            .map_or("", |name| self.db.symbol(name))
    }

    fn report(&mut self, span: Span, message: String) {
        self.nonconforming(span, message, Vec::new(), Vec::new());
    }

    fn nonconforming(
        &mut self,
        span: Span,
        message: String,
        labels: Vec<(UnitSpan, String)>,
        notes: Vec<String>,
    ) {
        let diag = Nonconforming {
            span,
            message,
            labels,
            notes,
        };
        self.diags.push((self.unit, Diag::new(diag)));
    }

    fn undecided(&mut self, span: Span, issue: Issue) {
        let residual = match issue {
            Issue::Residual(residual) => residual,
            Issue::Contradiction(_) => Residual::Unsupported("a contradicted member lookup"),
        };
        self.unresolved.push(Unresolved {
            span: UnitSpan {
                unit: self.unit,
                span,
            },
            residual,
        });
    }
}
