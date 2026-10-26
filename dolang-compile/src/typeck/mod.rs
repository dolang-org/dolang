//! Static checking of a set of compilation units.

mod analysis;
pub(crate) mod cfg;
#[cfg(test)]
mod diagnostics;
pub(crate) mod elab;
mod flow;
mod lower;
pub(crate) mod report;
pub(crate) mod solver;
#[cfg(feature = "debug")]
mod trace;
pub(crate) mod r#type;
pub(crate) mod typelib;
mod undecided;

use std::{borrow::Cow, num::NonZeroUsize, path::Path, sync::Arc};

use rustc_hash::FxHashSet;

use crate::{
    Error, ErrorInfo, Mode, Unit, UnitId,
    diag::{self, Diag, Severity},
};

pub use analysis::{Analysis, Declaration, Judged, Node};
pub use flow::Role;
pub use r#type::Style;

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
/// | [`ErrorKind::NotExportable`](crate::ErrorKind::NotExportable) | The module's public interface names a private declaration, which its diagnostics report |
pub fn typelib(unit: &Unit<'_>) -> Result<Vec<u8>, Error> {
    if unit.failed {
        return Err(Error(ErrorInfo::Fail));
    }
    if !unit.resolved {
        return Err(Error(ErrorInfo::Unresolved));
    }
    let Mode::Module { .. } = unit.mode else {
        return Err(Error(ErrorInfo::NotModule));
    };
    let harvest = elab::harvest(unit);
    let (kept, leaks) = harvest.interface();
    if !leaks.is_empty() {
        return Err(Error(ErrorInfo::NotExportable));
    }
    Ok(typelib::write(harvest, &kept))
}

/// A module's typelib, as [`typelib`] wrote it, decoded and found to be one this
/// checker reads. It borrows from the bytes it was read from.
///
/// A unit checked against a typelib (see [`Builder::typelib`]) sees the same
/// declarations as if the module were checked from source, but the module's bodies
/// are not checked. A typelib is read only by the version of the checker that wrote
/// it.
pub struct Typelib<'a> {
    harvest: elab::Harvest<'a>,
    /// The modules the module imports, each once, in order
    imports: Vec<&'a str>,
}

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
            .map(|(harvest, imports)| Self { harvest, imports })
            .map_err(|invalid| Error(ErrorInfo::Typelib(invalid)))
    }

    /// The module's name.
    pub fn module(&self) -> &'a str {
        self.harvest.info.module.expect("a typelib is a module's")
    }

    /// The path of the module's source when the typelib was written, which locates
    /// its diagnostics.
    pub fn path(&self) -> &'a Path {
        self.harvest.info.path
    }

    /// The modules the module imports, each once, in order. A check finds them
    /// among its units, and reports a module it doesn't have as not found.
    pub fn imports(&self) -> Vec<&'a str> {
        self.imports.clone()
    }
}

/// A unit to check
enum Input<'u, 's> {
    Source(&'u Unit<'s>),
    Typelib(Box<elab::Harvest<'u>>),
}

/// Collects the units to check together.
///
/// Each unit is identified by the [`UnitId`] returned when it is added; IDs
/// are meaningful only for the [`Check`] this builder produces.
pub struct Builder<'u, 's> {
    units: Vec<Input<'u, 's>>,
    modules: FxHashSet<&'u str>,
    /// The types `strand.PipeSender` and `strand.PipeReceiver` stand for, by module
    /// and item
    pipes: [(&'u str, &'u str); 2],
    /// The threads to check units on, if not rayon's global pool
    threads: Option<NonZeroUsize>,
    /// Whether to give the check an [`Analysis`]
    analysis: bool,
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
            modules: FxHashSet::default(),
            pipes: [("strand", "Sender"), ("strand", "Receiver")],
            threads: None,
            analysis: false,
        }
    }

    /// Check units on a pool of `threads` threads of their own. Defaults to rayon's
    /// global pool, which `RAYON_NUM_THREADS` sizes, else the available
    /// parallelism. The results don't depend on it.
    pub fn threads(&mut self, threads: NonZeroUsize) -> &mut Self {
        self.threads = Some(threads);
        self
    }

    /// Give the check an [`Analysis`] of its units checked from source, which
    /// [`Check::analysis`] borrows and [`Check::into_analysis`] takes. Defaults to
    /// off, which costs nothing.
    pub fn analysis(&mut self, enabled: bool) -> &mut Self {
        self.analysis = enabled;
        self
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
        if let Mode::Module { name } = &unit.mode {
            self.module(name)?;
        }
        self.units.push(Input::Source(unit));
        Ok(UnitId::from_index(self.units.len() - 1))
    }

    /// Add a module to check by its typelib. The same typelib may be added to any
    /// number of checks.
    ///
    /// Its types' well-formedness and its classes' conformance to their
    /// supertypes are not checked; that was done with its source.
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
        self.units
            .push(Input::Typelib(Box::new(typelib.harvest.clone())));
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
                Input::Typelib(harvest) => *harvest,
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
        let mut recursion = Vec::new();
        let recursion_unresolved = elab::recursion(&mut db, &tables, &mut recursion);
        // Every later pass runs in a fork per unit, so no unit's results depend on
        // what another interned
        let shared = db.share();
        let mut outputs = check_units(&shared, &tables, self.threads, self.analysis);
        // Gather each kind of output across units, in the order the passes ran
        let mut unresolved = Vec::new();
        for output in &mut outputs {
            diags.append(&mut output.wellformed.0);
            unresolved.append(&mut output.wellformed.1);
        }
        diags.append(&mut recursion);
        unresolved.extend(recursion_unresolved);
        for output in &mut outputs {
            diags.append(&mut output.overrides.0);
            unresolved.append(&mut output.overrides.1);
        }
        #[cfg(feature = "debug")]
        trace::elab(&r#type::Database::fork(&shared), &tables, &unresolved);
        // Each run's logged facts are copied into a database of the check's own,
        // which copies what the check's base holds once
        let mut results = r#type::Database::new();
        let mut copies = r#type::Copies::default();
        let mut cfgs = Vec::with_capacity(outputs.len());
        let mut layers = Vec::with_capacity(outputs.len());
        let mut gathered = Vec::with_capacity(outputs.len());
        for (index, output) in outputs.into_iter().enumerate() {
            let unit = UnitId::from_index(index);
            let flow = output.cfg.is_some().then(|| {
                let (facts, problems, undecided) = gather(output.runs, &mut results, &mut copies);
                for problem in &problems {
                    diags.push((unit, report::Diag::new(problem.clone())));
                }
                unresolved.extend(undecided.iter().map(|&(span, residual)| elab::Unresolved {
                    span: r#type::UnitSpan { unit, span },
                    residual,
                }));
                (facts, problems, undecided)
            });
            cfgs.push(output.cfg);
            layers.push(output.layer);
            gathered.push(flow);
        }
        results.seal();
        // Named now, so the results render without the units
        let labels = (results.declarations())
            .map(|(copy, _)| {
                let original =
                    (copies.origin(copy)).expect("every declaration in the results is a copy");
                let name = r#type::Names::declaration(&tables, original).into_owned();
                (
                    name.into_boxed_str(),
                    r#type::Names::collection(&tables, original),
                )
            })
            .collect();
        let flows: Vec<_> = (gathered.into_iter())
            .map(|flow| {
                flow.map(|(facts, problems, unresolved)| flow::Results {
                    facts: merge(&results, facts),
                    problems,
                    unresolved,
                })
            })
            .collect();
        let types = Arc::new(analysis::Types {
            db: results.share(),
            labels,
        });
        let analysis = (self.analysis).then(|| {
            let db = r#type::Database::fork(&types.db);
            let units = (flows.iter().zip(&cfgs).zip(&layers).enumerate())
                .map(|(index, ((flow, cfg), layer))| {
                    let unit = UnitId::from_index(index);
                    let (flow, cfg, layer) = (flow.as_ref()?, cfg.as_ref()?, layer.as_ref()?);
                    Some(tree(&tables, &db, unit, flow, cfg, layer))
                })
                .collect();
            // Placed now, so a member use's class is found without the units
            let places = db
                .declarations()
                .map(|(copy, _)| {
                    let original = copies
                        .origin(copy)
                        .expect("every declaration in the results is a copy");
                    let (module, name) = tables.place(original)?;
                    Some((module.into_boxed_str(), name.into_boxed_str()))
                })
                .collect();
            Analysis::new(Arc::clone(&types), units, places)
        });
        undecided::report(&tables.units, &unresolved, &mut diags);
        Check {
            cfgs,
            layers,
            flows,
            diagnostics: diags
                .iter()
                .map(|(unit, diag)| diag.resolve(*unit, &tables.units[unit.index()]))
                .collect(),
            tables,
            shared,
            types,
            analysis,
            unresolved,
        }
    }
}

/// What checking a unit in forks of its own concludes
struct UnitOutput {
    wellformed: (Vec<elab::UnitDiag>, Vec<elab::Unresolved>),
    overrides: (Vec<elab::UnitDiag>, Vec<elab::Unresolved>),
    cfg: Option<cfg::Ir>,
    /// The layer lowering interned in, which each flow run's fork is of
    layer: Option<Arc<r#type::Shared>>,
    /// Each flow run's fork, which its results are interned in, and its results,
    /// by run ID
    runs: Vec<(r#type::Database, flow::Results)>,
}

/// A unit's judgments, each variable named by its unit's source, and each
/// member by `results`, the check's own database its facts are interned in
fn tree(
    tables: &elab::Tables<'_>,
    results: &r#type::Database,
    unit: UnitId,
    flow: &flow::Results,
    cfg: &cfg::Ir,
    layer: &Arc<r#type::Shared>,
) -> analysis::Tree {
    let db = r#type::Database::fork(layer);
    let judgments = (flow.facts.iter())
        .map(|&((span, role), seen)| {
            let (mut decl, mut overload, mut item) = (None, None, None);
            let name = match seen.detail {
                flow::Detail::Var(var) => match cfg.var(var).origin {
                    cfg::Origin::Source(span) => Some(Cow::Borrowed(tables.text(unit, span))),
                    cfg::Origin::Field(key) => Some(Cow::Borrowed(db.symbol(key.name))),
                    _ => None,
                },
                flow::Detail::Member(key) => {
                    if let &r#type::Type::Decl(class) = results.ty(seen.fact.ty) {
                        decl = Some(class);
                    }
                    let name = results.symbol(key.name);
                    Some(match key.special {
                        true => Cow::Owned(format!("({name})")),
                        false => Cow::Borrowed(name),
                    })
                }
                flow::Detail::Overload(index) => {
                    overload = Some(index);
                    None
                }
                flow::Detail::Item(module, name) => {
                    let name = results.symbol(name);
                    let module = tables.units[module.index()].name();
                    item = Some((module.into_boxed_str(), Box::from(name)));
                    Some(Cow::Borrowed(name))
                }
                flow::Detail::None => None,
            };
            let entry = analysis::Entry {
                role,
                name: name.map(Box::from),
                ty: seen.fact.ty,
                unassigned: seen.fact.unassigned,
                decl,
                overload,
                item,
            };
            (span, entry)
        })
        .collect();
    analysis::Tree::new(judgments)
}

/// A unit's flow runs' facts, copied into `results` through `copies`, then their
/// problems and undecided checks, each run's in turn, in source order
#[expect(clippy::type_complexity, reason = "taken apart by the caller")]
fn gather(
    runs: Vec<(r#type::Database, flow::Results)>,
    results: &mut r#type::Database,
    copies: &mut r#type::Copies,
) -> (
    Vec<((crate::source::Span, flow::Role), flow::Seen)>,
    Vec<flow::Problem>,
    Vec<(crate::source::Span, solver::Residual)>,
) {
    let mut facts = Vec::new();
    let mut problems = Vec::new();
    let mut unresolved = Vec::new();
    for (db, mut run) in runs {
        let mut copier = results.copier(&db, copies);
        facts.extend((run.facts.into_iter()).map(|(key, mut seen)| {
            seen.fact.ty = copier.ty(seen.fact.ty);
            match &mut seen.detail {
                flow::Detail::Member(member) => member.name = copier.symbol(member.name),
                flow::Detail::Item(_, name) => *name = copier.symbol(*name),
                _ => {}
            }
            (key, seen)
        }));
        problems.append(&mut run.problems);
        unresolved.append(&mut run.unresolved);
    }
    flow::sort(&mut problems, &mut unresolved);
    (facts, problems, unresolved)
}

/// Facts by span and role, joined where runs logged facts at one
fn merge(
    db: &r#type::Database,
    mut facts: Vec<((crate::source::Span, flow::Role), flow::Seen)>,
) -> Vec<((crate::source::Span, flow::Role), flow::Seen)> {
    let gas = solver::Gas::default();
    let solver = solver::Solver::new(db, &gas);
    flow::collate(&mut facts, |old, new| solver.lub(old, new));
    facts
}

// Units are checked on other threads than the one that shares them
const _: () = {
    const fn send<T: Send>() {}
    const fn sync<T: Sync>() {}
    const fn owned<T: Send + 'static>() {}
    send::<UnitOutput>();
    sync::<elab::Tables<'_>>();
    // A unit's runs are analyzed on other threads than the one that lowered it
    send::<r#type::Database>();
    send::<flow::Results>();
    sync::<cfg::Ir>();
    sync::<flow::Layout>();
    // A check's results outlive its units, and are shared among the threads
    // that query them
    owned::<Analysis>();
    sync::<Analysis>();
};

/// Check every unit with [`check_unit`], on a pool of `threads` threads, else the
/// global pool, giving their outputs by [`UnitId`]. Flow logs facts only for an
/// `analysis`.
fn check_units(
    shared: &Arc<r#type::Shared>,
    tables: &elab::Tables<'_>,
    threads: Option<NonZeroUsize>,
    analysis: bool,
) -> Vec<UnitOutput> {
    let count = tables.units.len();
    let mut slots: Vec<Option<UnitOutput>> = (0..count).map(|_| None).collect();
    let mut work: Vec<_> = (slots.iter_mut().enumerate())
        .map(|(index, slot)| (UnitId::from_index(index), slot))
        .collect();
    // Largest first, so that no large unit starts last; typelibs have nothing of
    // their own to check
    work.sort_by_key(|(unit, _)| {
        let info = &tables.units[unit.index()];
        std::cmp::Reverse(info.source.map_or(0, |source| source.file.newlines().len()))
    });
    // FIFO, so that the worker spawning the units takes them largest first too
    let run = || {
        rayon::scope_fifo(|scope| {
            for (unit, slot) in work {
                scope.spawn_fifo(move |_| *slot = Some(check_unit(shared, tables, unit, analysis)));
            }
        })
    };
    // With traces, one thread, so that they don't interleave
    let threads = if cfg!(feature = "debug") && std::env::var_os("DOLANG_DEBUG").is_some() {
        NonZeroUsize::new(1)
    } else {
        threads
    };
    match threads {
        Some(threads) if !cfg!(target_family = "wasm") => rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get().min(count).max(1))
            .build()
            .expect("the pool's threads start")
            .install(run),
        // The global pool, which runs everything on this thread without threads to
        // spawn
        _ => run(),
    }
    (slots.into_iter())
        .map(|output| output.expect("every unit is checked"))
        .collect()
}

/// Check a unit with source, its declarations and bodies, in a new fork of
/// `shared`; a typelib's were checked with its source. Flow logs facts only for
/// an `analysis`.
fn check_unit(
    shared: &Arc<r#type::Shared>,
    tables: &elab::Tables<'_>,
    unit: UnitId,
    analysis: bool,
) -> UnitOutput {
    // A fork of its own, so that what it interns isn't in the layer
    let db = r#type::Database::fork(shared);
    let source = tables.units[unit.index()].source;
    // A typelib's well-formedness and overrides were checked with its source;
    // the solver copes with an ill-formed type, and a nonconforming member only
    // misleads checks against it
    let mut wellformed = Vec::new();
    let wellformed_unresolved = match source {
        Some(_) => elab::wellformed(&db, tables, unit, &mut wellformed),
        None => Vec::new(),
    };
    let mut overrides = Vec::new();
    let overrides_unresolved = match source {
        Some(_) => elab::overrides(&db, tables, unit, &mut overrides),
        None => Vec::new(),
    };
    // A unit's bodies are checked only from its source, in runs that each have a
    // fork of the layer lowering interned in
    let (cfg, layer, runs) = match source {
        Some(_) => {
            let db = r#type::Database::fork(shared);
            let ir = lower::lower(tables, &db, unit);
            debug_assert_eq!(ir.validate(), Ok(()), "lowering builds a valid graph");
            #[cfg(debug_assertions)]
            ir.check_stack_depths();
            #[cfg(feature = "debug")]
            if let Err(e) = export_dot(&ir, &db, &tables.units[unit.index()]) {
                dolang_util::debug_eprintln!(topic: "dot", "Typing CFG DOT export failed: {e}");
            }
            let layer = db.share();
            let layout = flow::Layout::new(&ir);
            let runs = flow::analyze(&ir, &layout, &ir.runs(), &layer, tables, analysis);
            (Some(ir), Some(layer), runs)
        }
        None => (None, None, Vec::new()),
    };
    UnitOutput {
        wellformed: (wellformed, wellformed_unresolved),
        overrides: (overrides, overrides_unresolved),
        cfg,
        layer,
        runs,
    }
}

/// Export a unit's typing CFG to a DOT file under `DOLANG_EXPORT_DOT`, if it is set
#[cfg(feature = "debug")]
fn export_dot(ir: &cfg::Ir, db: &r#type::Database, info: &elab::UnitInfo) -> std::io::Result<()> {
    let Some(out) = crate::dot_path(info.path, "typeck.dot")? else {
        return Ok(());
    };
    let file = &info.source.expect("a lowered unit has a source").file;
    ir.dot(db, |span| file.str(span), &mut std::fs::File::create(&out)?)?;
    dolang_util::debug_eprintln!(topic: "dot", "Typing CFG DOT exported to: {}", out.display());
    Ok(())
}

/// The result of checking a set of units.
///
/// A check is *validated* when the checker reported no errors. A check it
/// couldn't decide in a strict unit is a warning; in a non-strict unit, which
/// accepts `Unknown`, it isn't reported. Otherwise the check is *partial*: its
/// declarations are usable for diagnostics and tooling, but checking code against
/// them proves nothing. Units that could not
/// be checked at all are refused by [`Builder::unit`].
pub struct Check<'u> {
    diagnostics: Vec<Diag>,
    tables: elab::Tables<'u>,
    /// The database every unit's fork is of
    shared: Arc<r#type::Shared>,
    /// A database of the check's own, which flow's facts are copied into, and the
    /// names of its declarations
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    types: Arc<analysis::Types>,
    /// What the check concluded about its units' spans, if the builder asked
    analysis: Option<Analysis>,
    /// Well-formedness checks the checker could not decide
    unresolved: Vec<elab::Unresolved>,
    /// Each unit's typing CFG, by [`UnitId`], for a unit checked from source
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    cfgs: Vec<Option<cfg::Ir>>,
    /// The layer each unit's flow runs forked, by [`UnitId`], for a unit checked
    /// from source
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    layers: Vec<Option<Arc<r#type::Shared>>>,
    /// What flow analysis concluded about each unit, by [`UnitId`], for a unit checked
    /// from source, its facts' types interned in `types`
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
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

    /// The module name of a checked unit, as an [`Analysis`] names the modules
    /// declarations are in. A script is named by its file stem.
    pub fn name(&self, unit: UnitId) -> String {
        self.tables.units[unit.index()].name()
    }

    /// Whether every well-formedness check passed. See [`Check`].
    pub fn validated(&self) -> bool {
        (self.diagnostics.iter()).all(|diag| diag.severity() != Severity::Error)
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

    /// What the check concluded about its units' spans, if the builder asked for it
    /// with [`Builder::analysis`].
    pub fn analysis(&self) -> Option<&Analysis> {
        self.analysis.as_ref()
    }

    /// Take what the check concluded about its units' spans, if the builder asked
    /// for it with [`Builder::analysis`].
    pub fn into_analysis(self) -> Option<Analysis> {
        self.analysis
    }

    /// The judgments about spans of `unit`, in source order. The check must have an
    /// [`Analysis`].
    #[doc(hidden)]
    pub fn judgments(&self, unit: UnitId) -> Vec<Judgment> {
        let info = &self.tables.units[unit.index()];
        let db = &r#type::Database::fork(&self.shared);
        let mut judgments = self.tables.judgments(db, unit, &self.unresolved);
        let analysis = (self.analysis.as_ref()).expect("judgments are rendered from an analysis");
        for node in analysis.nodes(unit) {
            judgments.extend(node.judgments().map(|judged| {
                let ty = judged.ty(Style::Full);
                let name = match judged.role() {
                    Role::Read | Role::Bind => "flow",
                    Role::Value => "value",
                    Role::Expected => "expected",
                    Role::Member => "reached",
                    Role::Callee => "callee",
                    Role::Item => "item",
                };
                let value = match (judged.role(), judged.unassigned(), judged.none()) {
                    (Role::Member, ..) => format!("{ty}.{}", judged.name().unwrap_or("?")),
                    (Role::Item, ..) => match judged.declaration() {
                        Some(item) => format!("{}.{}", item.module(), item.name()),
                        None => "?".to_owned(),
                    },
                    (Role::Callee, ..) => match judged.overload() {
                        Some(index) => format!("overload {index}: {ty}"),
                        None => ty,
                    },
                    (_, false, _) => ty,
                    (_, true, true) => "unassigned".to_owned(),
                    (_, true, false) => format!("{ty} | unassigned"),
                };
                (name, node.span(), value)
            }));
        }
        judgments.sort_by_key(|&(name, span, _)| (span.start, span.end, name));
        // A union's alternatives each reach a member, and a call is made once
        // for each alternative of a union callee
        judgments.dedup_by(|(name, span, value), (kept_name, kept_span, kept)| {
            let same =
                (*name, *span) == (*kept_name, *kept_span) && matches!(*name, "reached" | "callee");
            if same {
                kept.push_str(" | ");
                kept.push_str(value);
            }
            same
        });
        judgments
            .into_iter()
            .map(|(name, span, value)| Judgment {
                name,
                span: report::resolve_span(unit, info, span).span(),
                value,
            })
            .collect()
    }

    /// A fact's type, interned in the results
    #[cfg(test)]
    fn render_fact(&self, ty: r#type::TypeId) -> String {
        self.types.render(ty, Style::Full)
    }

    /// Relate every type the database holds to itself and to top, interpreting each
    /// binder as the dynamic type or schema of its kind, to show that the solver can
    /// judge the sealed database without panicking. The outcomes are discarded.
    #[doc(hidden)]
    pub fn smoke(&self) {
        use solver::{Gas, Provenance, Solver};

        let db = &r#type::Database::fork(&self.shared);
        let gas = Gas::default();
        let mut solver = Solver::new(db, &gas);
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
        use solver::{Gas, Solver};
        use r#type::{Element, Member, Scope, Type};

        let db = &r#type::Database::fork(&self.shared);
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
            let gas = Gas::default();
            let mut solver = Solver::new(db, &gas);
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
