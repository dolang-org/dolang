//! Static checking of a set of compilation units.

#[allow(dead_code, reason = "built by lowering (#735)")]
pub(crate) mod cfg;
pub(crate) mod elab;
pub(crate) mod solver;
pub(crate) mod r#type;

use std::collections::HashSet;

use crate::{
    Error, ErrorInfo, Mode, Unit, UnitId,
    diag::{self, Diag, Severity},
    source,
};

/// Collects the units to check together.
///
/// Each unit is identified by the [`UnitId`] returned when it is added; IDs
/// are meaningful only for the [`Check`] this builder produces.
pub struct Builder<'u, 's> {
    units: Vec<&'u Unit<'s>>,
    modules: HashSet<&'u str>,
}

impl Default for Builder<'_, '_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'u, 's> Builder<'u, 's> {
    pub fn new() -> Self {
        Self {
            units: Vec::new(),
            modules: HashSet::new(),
        }
    }

    /// Add a unit to check.
    ///
    /// The unit must have been compiled with [`Config::typecheck`](crate::Config::typecheck).
    ///
    /// # Errors
    ///
    /// | Kind | Condition |
    /// | ---- | --------- |
    /// | [`ErrorKind::Fail`](crate::ErrorKind::Fail) | The unit failed to compile |
    /// | [`ErrorKind::Unresolved`](crate::ErrorKind::Unresolved) | The unit was compiled without resolving types |
    /// | [`ErrorKind::DuplicateModule`](crate::ErrorKind::DuplicateModule) | A module of the same name was already added |
    pub fn unit(&mut self, unit: &'u Unit<'s>) -> Result<UnitId, Error> {
        if unit.failed {
            return Err(Error(ErrorInfo::Fail));
        }
        if !unit.resolved {
            return Err(Error(ErrorInfo::Unresolved));
        }
        let id = UnitId::from_index(self.units.len());
        if let Mode::Module { name } = unit.compiler.mode
            && !self.modules.insert(name)
        {
            return Err(Error(ErrorInfo::DuplicateModule(name.to_owned())));
        }
        self.units.push(unit);
        Ok(id)
    }

    /// Check the units.
    ///
    /// Units are checked in a fixed order, modules by name and then scripts by path,
    /// whatever order they were added in.
    pub fn check(self) -> Check<'u> {
        let mut db = r#type::Database::new();
        for index in 0..self.units.len() {
            assert_eq!(
                db.allocate_unit().index(),
                index,
                "units are allocated in order"
            );
        }
        let units: Vec<&Unit<'_>> = self.units;
        let mut order: Vec<UnitId> = (0..units.len()).map(UnitId::from_index).collect();
        order.sort_by_key(|id| {
            let compiler = &units[id.index()].compiler;
            match compiler.mode {
                Mode::Module { name } => (0, name, None),
                Mode::Script | Mode::Repl => (1, "", Some(compiler.file.path())),
            }
        });
        let (mut tables, mut diags) = elab::collect(&mut db, &units, &order);
        elab::kinds(&mut tables, &mut diags);
        elab::signatures(&mut tables, &mut diags);
        elab::captures(&mut tables);
        elab::variances(&mut tables);
        elab::populate(&mut db, &mut tables, &mut diags);
        db.seal();
        elab::specialize(&mut db, &tables, &mut diags);
        let unresolved = elab::wellformed(&db, &tables, &mut diags);
        Check {
            diagnostics: diags
                .iter()
                .map(|(unit, diag)| diag.resolve_in(&units[unit.index()].compiler, Some(*unit)))
                .collect(),
            tables,
            db,
            unresolved,
        }
    }
}

/// The result of checking a set of units.
///
/// A check is *validated* when every well-formedness check passed: the checker
/// reported no errors, and could decide every check it ran. Otherwise it is
/// *partial*: its declarations are usable for diagnostics and tooling, but
/// checking code against them proves nothing. Units that could not be checked at
/// all are refused by [`Builder::unit`].
pub struct Check<'u> {
    diagnostics: Vec<Diag>,
    tables: elab::Tables<'u>,
    db: r#type::Database,
    /// Well-formedness checks the checker could not decide
    unresolved: Vec<elab::Unresolved>,
}

/// The names of the judgments [`Check::judgments`] reports.
#[doc(hidden)]
pub const JUDGMENTS: &[&str] = elab::JUDGMENTS;

/// A fact the checker concluded about a span, for regression tests.
#[doc(hidden)]
pub struct Judgment {
    pub name: &'static str,
    pub span: diag::Span,
    pub value: String,
}

impl Check<'_> {
    /// Iterate the type checker's diagnostics.
    ///
    /// Every location names the unit it refers to. The units' own diagnostics
    /// are not included.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diag> {
        self.diagnostics.iter()
    }

    /// Whether every well-formedness check passed. See [`Check`].
    pub fn validated(&self) -> bool {
        self.unresolved.is_empty()
            && self
                .diagnostics
                .iter()
                .all(|diag| diag.severity() != Severity::Error)
    }

    /// The judgments about spans of `unit`, in source order.
    #[doc(hidden)]
    pub fn judgments(&self, unit: UnitId) -> Vec<Judgment> {
        let compiler = &self.tables.units[unit.index()].compiler;
        self.tables
            .judgments(&self.db, unit, &self.unresolved)
            .into_iter()
            .map(|(name, span, value)| Judgment {
                name,
                span: source::Diag::resolve_span(compiler, span),
                value,
            })
            .collect()
    }

    /// Relate every type the database holds to itself and to top, interpreting each
    /// binder as the dynamic type or schema of its kind, to show that the solver can
    /// judge the sealed database without panicking. The outcomes are discarded.
    #[doc(hidden)]
    pub fn smoke(&self) {
        use solver::{Provenance, Solver};

        let db = &self.db;
        let mut solver = Solver::new(db);
        let relate = |solver: &mut Solver<'_>, ty, kinds: &[r#type::Kind]| {
            let group = kinds
                .iter()
                .map(|&kind| solver.closed(db.unknown_of(kind)))
                .collect();
            let environment = solver.environment(solver.empty_environment(), group);
            let view = solver.view(ty, environment);
            solver.constrain(view, view, Provenance::default());
            if db.kind(ty) == r#type::Kind::Type {
                solver.constrain(view, solver.closed(db.top()), Provenance::default());
            }
        };
        for (_, declaration) in db.declarations() {
            let kinds: Vec<_> = match db.ty(declaration.ty) {
                r#type::Type::Quantified { binders, .. } => {
                    binders.iter().map(|binder| binder.kind).collect()
                }
                _ => Vec::new(),
            };
            let body = match db.ty(declaration.ty) {
                r#type::Type::Quantified { body, .. } => *body,
                _ => declaration.ty,
            };
            relate(&mut solver, body, &kinds);
            for &supertype in declaration.supertypes.iter() {
                relate(&mut solver, supertype, &kinds);
            }
            for (_, member) in declaration.members.iter() {
                if let r#type::Member::Field { ty, .. } = member {
                    relate(&mut solver, *ty, &kinds);
                }
            }
        }
        for (ty, kinds) in self.tables.site_kinds() {
            relate(&mut solver, ty, &kinds);
        }
        solver.solve();
        self.smoke_members();
    }

    /// Look up every member a class or its direct supertypes declare, through the
    /// receiver of one of its instance methods under that method's rigids.
    fn smoke_members(&self) {
        use solver::Solver;
        use r#type::{Element, Member, Scope, Type};

        let db = &self.db;
        let declared = |ty| match *db.ty(ty) {
            Type::Decl(decl) => Some(decl),
            Type::Apply { base, .. } => match *db.ty(base) {
                Type::Decl(decl) => Some(decl),
                _ => None,
            },
            _ => None,
        };
        for (class, declaration) in db.declarations() {
            let mut keys: Vec<_> = declaration.members.iter().map(|(key, _)| *key).collect();
            for decl in declaration.supertypes.iter().filter_map(|&ty| declared(ty)) {
                let members = &db.declaration(decl).members;
                keys.extend(
                    members
                        .iter()
                        .map(|(key, _)| *key)
                        .filter(|key| !key.private),
                );
            }
            let receiver = declaration.members.iter().find_map(|(_, member)| {
                let Member::Method {
                    decl,
                    scope: Scope::Instance,
                    ..
                } = *member
                else {
                    return None;
                };
                let mut ty = db.declaration(decl).ty;
                if let Type::Quantified { body, .. } = db.ty(ty) {
                    ty = *body;
                }
                let Type::Function(function) = db.ty(ty) else {
                    return None;
                };
                let Type::Schema(items) = db.ty(function.params) else {
                    return None;
                };
                match items.first()?.element {
                    Element::Positional(receiver) => Some((decl, receiver)),
                    _ => None,
                }
            });
            let Some((method, receiver)) = receiver else {
                continue;
            };
            let mut solver = Solver::new(db);
            let environment = solver.rigid_environment(method);
            let receiver = solver.view(receiver, environment);
            for key in keys {
                let _ = if key.private {
                    solver.private_member(receiver, class, key)
                } else {
                    solver.member(receiver, key)
                };
            }
        }
    }
}
