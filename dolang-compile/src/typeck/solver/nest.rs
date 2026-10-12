//! What solvers nested in one another share (#900)

use std::cell::Cell;

use super::Residual;

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
