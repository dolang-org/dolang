//! Static checking of a set of compilation units.

pub(crate) mod cfg;
pub(crate) mod elab;
mod flow;
mod lower;
pub(crate) mod report;
pub(crate) mod solver;
pub(crate) mod r#type;
pub(crate) mod typelib;

use std::{collections::HashSet, path::Path};

use crate::{
    Error, ErrorInfo, Mode, Unit, UnitId,
    diag::{self, Diag, Severity},
};

/// Write a module's typelib: its declarations as the checker reads them, without its
/// source. Read it back with [`Typelib::read`].
///
/// The unit must have been compiled with [`Config::typecheck`](crate::Config::typecheck).
///
/// # Errors
///
/// | Kind | Condition |
/// | ---- | --------- |
/// | [`ErrorKind::Fail`](crate::ErrorKind::Fail) | The unit failed to compile |
/// | [`ErrorKind::Unresolved`](crate::ErrorKind::Unresolved) | The unit was compiled without resolving types |
/// | [`ErrorKind::NotModule`](crate::ErrorKind::NotModule) | The unit is not a module |
pub fn typelib(unit: &Unit<'_>) -> Result<Vec<u8>, Error> {
    if unit.failed {
        return Err(Error(ErrorInfo::Fail));
    }
    if !unit.resolved {
        return Err(Error(ErrorInfo::Unresolved));
    }
    let Mode::Module { .. } = unit.compiler.mode else {
        return Err(Error(ErrorInfo::NotModule));
    };
    Ok(typelib::write(elab::harvest(unit)))
}

/// A module's typelib, as [`typelib`] wrote it, decoded and found to be one this
/// checker reads. It borrows from the bytes it was read from.
///
/// A unit checked against a typelib (see [`Builder::typelib`]) sees the same
/// declarations as if the module were checked from source, but the module's bodies
/// are not checked. A typelib is read only by the version of the checker that wrote
/// it.
pub struct Typelib<'a>(elab::Harvest<'a>);

impl<'a> Typelib<'a> {
    /// Read a typelib.
    ///
    /// # Errors
    ///
    /// | Kind | Condition |
    /// | ---- | --------- |
    /// | [`ErrorKind::Typelib`](crate::ErrorKind::Typelib) | The bytes are not a typelib this version of the checker reads |
    pub fn read(bytes: &'a [u8]) -> Result<Self, Error> {
        typelib::read(bytes)
            .map(Self)
            .map_err(|invalid| Error(ErrorInfo::Typelib(invalid)))
    }

    /// The module's name.
    pub fn module(&self) -> &'a str {
        self.0.info.module.expect("a typelib is a module's")
    }

    /// The path of the module's source when the typelib was written, which locates
    /// its diagnostics.
    pub fn path(&self) -> &'a Path {
        self.0.info.path
    }

    /// The modules the module's declarations name, each once, in order. A check finds
    /// what they name among its units, and treats a module it doesn't have as
    /// unknown.
    pub fn imports(&self) -> Vec<&'a str> {
        use elab::Target;

        let targets = (self.0.pending.iter().map(|pending| &pending.base))
            .chain(self.0.exports.values().map(|(_, target)| target));
        let mut imports: Vec<&'a str> = targets
            .filter_map(|target| match *target {
                Target::Import { module, .. } | Target::Module(module) => Some(module),
                Target::Local(_) => None,
            })
            .collect();
        imports.sort_unstable();
        imports.dedup();
        imports
    }
}

/// A unit to check
enum Input<'u, 's> {
    Source(&'u Unit<'s>),
    Typelib(elab::Harvest<'u>),
}

/// Collects the units to check together.
///
/// Each unit is identified by the [`UnitId`] returned when it is added; IDs
/// are meaningful only for the [`Check`] this builder produces.
pub struct Builder<'u, 's> {
    units: Vec<Input<'u, 's>>,
    modules: HashSet<&'u str>,
    /// The types `strand.PipeSender` and `strand.PipeReceiver` stand for, by module
    /// and item
    pipes: [(&'u str, &'u str); 2],
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
            pipes: [("strand", "Sender"), ("strand", "Receiver")],
        }
    }

    /// Nominate the types `strand.PipeSender` and `strand.PipeReceiver` stand for,
    /// each by module and item: the pipes the embedding gives `strand.stream` and
    /// `strand.pipeline` stages.
    ///
    /// Each placeholder becomes an alias of its nominee applied to the
    /// placeholder's type arguments. A nominee whose module is not checked leaves
    /// the placeholder dynamic. Defaults to `strand.Sender` and `strand.Receiver`.
    pub fn pipes(&mut self, sender: (&'u str, &'u str), receiver: (&'u str, &'u str)) -> &mut Self {
        self.pipes = [sender, receiver];
        self
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
        if let Mode::Module { name } = unit.compiler.mode {
            self.module(name)?;
        }
        self.units.push(Input::Source(unit));
        Ok(UnitId::from_index(self.units.len() - 1))
    }

    /// Add a module to check by its typelib. The same typelib may be added to any
    /// number of checks.
    ///
    /// Diagnostics may locate spans of the module, by line and column; it has no
    /// source to show.
    ///
    /// # Errors
    ///
    /// | Kind | Condition |
    /// | ---- | --------- |
    /// | [`ErrorKind::DuplicateModule`](crate::ErrorKind::DuplicateModule) | A module of the same name was already added |
    pub fn typelib(&mut self, typelib: &Typelib<'u>) -> Result<UnitId, Error> {
        self.module(typelib.module())?;
        self.units.push(Input::Typelib(typelib.0.clone()));
        Ok(UnitId::from_index(self.units.len() - 1))
    }

    fn module(&mut self, name: &'u str) -> Result<(), Error> {
        match self.modules.insert(name) {
            true => Ok(()),
            false => Err(Error(ErrorInfo::DuplicateModule(name.to_owned()))),
        }
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
        let harvests: Vec<elab::Harvest<'u>> = (self.units.into_iter())
            .map(|input| match input {
                Input::Source(unit) => elab::harvest(unit),
                Input::Typelib(harvest) => harvest,
            })
            .collect();
        let mut order: Vec<UnitId> = (0..harvests.len()).map(UnitId::from_index).collect();
        order.sort_by_key(|id| {
            let info = &harvests[id.index()].info;
            match info.module {
                Some(name) => (0, name, None),
                None => (1, "", Some(info.path)),
            }
        });
        let (mut tables, mut diags) = elab::link(&mut db, harvests, &order, self.pipes);
        elab::kinds(&mut tables, &mut diags);
        elab::signatures(&mut tables, &mut diags);
        elab::captures(&mut tables);
        elab::variances(&mut tables);
        elab::populate(&mut db, &mut tables, &mut diags);
        db.seal();
        elab::specialize(&mut db, &tables, &mut diags);
        let mut unresolved = elab::wellformed(&db, &tables, &mut diags);
        unresolved.extend(elab::overrides(&db, &tables, &mut diags));
        // A unit's bodies are checked only from its source
        let cfgs = (0..tables.units.len())
            .map(|index| {
                tables.units[index].source?;
                let ir = lower::lower(&tables, &db, UnitId::from_index(index));
                debug_assert_eq!(ir.validate(), Ok(()), "lowering builds a valid graph");
                #[cfg(debug_assertions)]
                ir.check_stack_depths();
                #[cfg(feature = "debug")]
                if let Err(e) = export_dot(&ir, &db, &tables.units[index]) {
                    dolang_util::debug_eprintln!(topic: "dot", "Typing CFG DOT export failed: {e}");
                }
                Some(ir)
            })
            .collect::<Vec<_>>();
        let flows: Vec<Option<flow::Results>> = cfgs
            .iter()
            .map(|ir| ir.as_ref().map(|ir| flow::analyze(ir, &db, &tables)))
            .collect();
        for (index, results) in flows.iter().enumerate() {
            let Some(results) = results else {
                continue;
            };
            let unit = UnitId::from_index(index);
            for problem in &results.problems {
                diags.push((unit, report::Diag::new(problem.clone())));
            }
            unresolved.extend(results.unresolved.iter().map(|&(span, residual)| {
                elab::Unresolved {
                    span: r#type::UnitSpan { unit, span },
                    residual,
                }
            }));
        }
        Check {
            cfgs,
            flows,
            diagnostics: diags
                .iter()
                .map(|(unit, diag)| diag.resolve(*unit, &tables.units[unit.index()]))
                .collect(),
            tables,
            db,
            unresolved,
        }
    }
}

/// Export a unit's typing CFG to a DOT file under `DOLANG_EXPORT_DOT`, if it is set
#[cfg(feature = "debug")]
fn export_dot(ir: &cfg::Ir, db: &r#type::Database, info: &elab::UnitInfo) -> std::io::Result<()> {
    let Some(out) = crate::dot_path(info.path, "typeck.dot")? else {
        return Ok(());
    };
    let file = &info.source.expect("a lowered unit has a source").compiler.file;
    ir.dot(db, |span| file.str(span), &mut std::fs::File::create(&out)?)?;
    dolang_util::debug_eprintln!(topic: "dot", "Typing CFG DOT exported to: {}", out.display());
    Ok(())
}

/// The result of checking a set of units.
///
/// A check is *validated* when every well-formedness check passed: the checker
/// reported no errors, and could decide every check it ran, except those that
/// need a form it doesn't support yet. Those are provisionally accepted.
/// Otherwise it is *partial*: its declarations are usable for diagnostics and
/// tooling, but checking code against them proves nothing. Units that could not
/// be checked at all are refused by [`Builder::unit`].
pub struct Check<'u> {
    diagnostics: Vec<Diag>,
    tables: elab::Tables<'u>,
    db: r#type::Database,
    /// Well-formedness checks the checker could not decide
    unresolved: Vec<elab::Unresolved>,
    /// Each unit's typing CFG, by [`UnitId`], for a unit checked from source
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    cfgs: Vec<Option<cfg::Ir>>,
    /// What flow analysis concluded about each unit, by [`UnitId`], for a unit checked
    /// from source
    flows: Vec<Option<flow::Results>>,
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

    /// The path of a checked unit: the one it was compiled with, or for a typelib,
    /// its module's when the typelib was written.
    pub fn path(&self, unit: UnitId) -> &Path {
        self.tables.units[unit.index()].path
    }

    /// Whether every well-formedness check passed. See [`Check`].
    pub fn validated(&self) -> bool {
        self.unresolved
            .iter()
            .all(|unresolved| matches!(unresolved.residual, solver::Residual::Unsupported(_)))
            && self
                .diagnostics
                .iter()
                .all(|diag| diag.severity() != Severity::Error)
    }

    /// The checks the checker could not decide, each with the kind of reason,
    /// for developing the checker. Reasons are internal and may change.
    #[doc(hidden)]
    pub fn undecided(&self) -> Vec<(String, diag::SourceSpan)> {
        self.unresolved
            .iter()
            .map(|unresolved| {
                let unit = unresolved.span.unit;
                let info = &self.tables.units[unit.index()];
                (
                    format!("{:?}", unresolved.residual),
                    report::resolve_span(unit, info, unresolved.span.span),
                )
            })
            .collect()
    }

    /// The judgments about spans of `unit`, in source order.
    #[doc(hidden)]
    pub fn judgments(&self, unit: UnitId) -> Vec<Judgment> {
        let info = &self.tables.units[unit.index()];
        let mut judgments = self.tables.judgments(&self.db, unit, &self.unresolved);
        let facts = self.flows[unit.index()].iter().flat_map(|flow| &flow.facts);
        judgments.extend(facts.map(|(&span, fact)| {
            let ty = self.tables.render_type(&self.db, fact.ty);
            let value = match (fact.unassigned, fact.ty == self.db.bottom()) {
                (false, _) => ty,
                (true, true) => "unassigned".to_owned(),
                (true, false) => format!("{ty} | unassigned"),
            };
            ("flow", span, value)
        }));
        judgments.sort_by_key(|&(name, span, _)| (span.start, span.end, name));
        judgments
            .into_iter()
            .map(|(name, span, value)| Judgment {
                name,
                span: report::resolve_span(unit, info, span).span(),
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
            for supertype in declaration.supertypes.iter() {
                relate(&mut solver, supertype.ty, &kinds);
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
            for decl in declaration
                .supertypes
                .iter()
                .filter_map(|supertype| declared(supertype.ty))
            {
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
