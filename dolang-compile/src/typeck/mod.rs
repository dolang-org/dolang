//! Static checking of a set of compilation units.

pub(crate) mod elab;
pub(crate) mod solver;
pub(crate) mod r#type;

use std::collections::HashSet;

use crate::{Error, ErrorInfo, Mode, Unit, UnitId, diag::Diag};

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
    pub fn check(self) -> Check {
        let mut db = r#type::Database::new();
        for index in 0..self.units.len() {
            assert_eq!(
                db.allocate_unit().index(),
                index,
                "units are allocated in order"
            );
        }
        let units: Vec<&Unit<'_>> = self.units;
        // The tables are what later stages of elaboration will consume
        let (_tables, diags) = elab::collect(&mut db, &units);
        Check {
            diagnostics: diags
                .iter()
                .map(|(unit, diag)| diag.resolve_in(&units[unit.index()].compiler, Some(*unit)))
                .collect(),
        }
    }
}

/// The result of checking a set of units.
pub struct Check {
    diagnostics: Vec<Diag>,
}

impl Check {
    /// Iterate the type checker's diagnostics.
    ///
    /// Every location names the unit it refers to. The units' own diagnostics
    /// are not included.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diag> {
        self.diagnostics.iter()
    }
}
