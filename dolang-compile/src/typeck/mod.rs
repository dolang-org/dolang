//! Static checking of a set of compilation units.

pub(crate) mod elab;
pub(crate) mod solver;
pub(crate) mod r#type;

use std::collections::HashSet;

use crate::{
    Error, ErrorInfo, Mode, Unit, UnitId,
    diag::{self, Diag},
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
        elab::variances(&mut tables);
        Check {
            diagnostics: diags
                .iter()
                .map(|(unit, diag)| diag.resolve_in(&units[unit.index()].compiler, Some(*unit)))
                .collect(),
            tables,
        }
    }
}

/// The result of checking a set of units.
pub struct Check<'u> {
    diagnostics: Vec<Diag>,
    tables: elab::Tables<'u>,
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

    /// The judgments about spans of `unit`, in source order.
    #[doc(hidden)]
    pub fn judgments(&self, unit: UnitId) -> Vec<Judgment> {
        let compiler = &self.tables.units[unit.index()].compiler;
        self.tables
            .judgments(unit)
            .into_iter()
            .map(|(name, span, value)| Judgment {
                name,
                span: source::Diag::resolve_span(compiler, span),
                value,
            })
            .collect()
    }
}
