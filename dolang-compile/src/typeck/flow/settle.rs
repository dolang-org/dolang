//! Settling a method's decorator region: what its decorators give, found by a
//! flow run over the region's blocks alone.
//!
//! The run starts at the region's entry, where every annotated variable of the
//! module holds its annotation and every other one is unassigned, and stops at
//! its exit. It iterates and defaults as any run does, so the application is
//! typed by flow's own call rule. Its final pass reports the region's problems.
//!
//! A region doesn't settle if any of these happen:
//! - it creates a function, as a `do` block does, whose body would need a run of
//!   its own;
//! - it has a handler;
//! - it reads a variable that's neither annotated nor assigned in the region;
//! - it reaches a member that a region not yet settled stands for.
//!
//! The run is abandoned where it's blocked. Only the last can change: the driver
//! runs the region again once that member settles.
//!
//! In a generic class, the method's value is its declared type at its class's
//! rigids, and what the region gives is abstracted back over the class's binders.
//! Each run has its own verdicts, so nothing it decides outlives it.

use std::{cell::Cell, sync::Arc};

use rustc_hash::FxHashSet;

use super::{Flow, Layout, Results, sort};
use crate::typeck::{
    cfg::{FuncId, Ir, Region, Run, Step, VarFlags},
    elab::Tables,
    flow::state::{CtxId, Group, May, State, VarState},
    solver::Verdicts,
    r#type::{Database, DeclId, SettledMember, Shared, Type, TypeId},
};

/// What a settle run settles
pub(super) struct Settling<'a> {
    pub(super) region: &'a Region,
    /// What first blocked it, if anything has
    blocked: Cell<Option<Blocked>>,
}

impl Settling<'_> {
    /// Note what blocks the run, unless something already has
    pub(super) fn block(&self, blocked: Blocked) {
        if self.blocked.get().is_none() {
            self.blocked.set(Some(blocked));
        }
    }
}

/// What blocks a settle run
#[derive(Clone, Copy)]
pub(super) enum Blocked {
    /// It read a variable that's neither annotated nor assigned in its region
    Read,
    /// It reached this decorated member, or accessor, not settled yet
    Member(DeclId),
}

/// What settling a region concludes
pub(crate) enum Settle {
    /// It settled: the run's fork, what the run concluded, and what the method's
    /// member settled to
    Settled(Box<(Database, Results, SettledMember)>),
    /// It reached this decorated member, or accessor, before it settled
    Blocked(DeclId),
    /// It can't settle
    Failed,
}

/// Settle a method's decorator region in a fork of `layer`, the unit's with the
/// members settled so far, from the unit's module run
pub(crate) fn settle(
    ir: &Ir,
    layout: &Layout,
    run: &Run,
    region: &Region,
    layer: &Arc<Shared>,
    tables: &Tables<'_>,
    logging: bool,
) -> Settle {
    if !confined(ir, region) {
        return Settle::Failed;
    }
    let db = Database::fork(layer);
    let verdicts = Verdicts::default();
    let mut flow = Flow::new(ir, layout, run, &db, &verdicts, tables, false);
    flow.logging = logging;
    flow.settling = Some(Settling {
        region,
        blocked: Cell::new(None),
    });
    // The method's value is seen at its class's rigids
    let class = region.class;
    let rigids = tables.group_rigids(&db, (class, 0));
    if !rigids.is_empty() && !flow.scope.iter().any(|(decl, _)| *decl == class) {
        flow.scope.push((class, rigids.clone()));
    }
    let (results, ty, callable) = match flow.settle_region() {
        Ok(Some(settled)) => settled,
        Ok(None) | Err(Blocked::Read) => return Settle::Failed,
        Err(Blocked::Member(decl)) => return Settle::Blocked(decl),
    };
    let ty = match rigids.is_empty() {
        true => ty,
        false => match db.abstract_rigids(ty, class) {
            Ok(ty) => ty,
            Err(_) => return Settle::Failed,
        },
    };
    let member = match callable {
        true => SettledMember::Method(ty),
        false => SettledMember::Value(ty),
    };
    Settle::Settled(Box::new((db, results, member)))
}

/// Whether a region's blocks create no function and have no handler
fn confined(ir: &Ir, region: &Region) -> bool {
    let mut seen = FxHashSet::default();
    let mut pending = vec![region.entry];
    while let Some(block) = pending.pop() {
        if block == region.exit || !seen.insert(block) {
            continue;
        }
        let data = ir.block(block);
        if data.handler.is_some()
            || data
                .steps
                .iter()
                .any(|step| matches!(step, Step::Capture(_)))
        {
            return false;
        }
        pending.extend(data.terminal.successors());
    }
    true
}

impl Flow<'_, '_> {
    /// Run a settle run's region from its entry, giving what its final pass
    /// concluded and the type bound to the region's result where it exits, with
    /// whether it's callable, if it reached the exit, unless it was blocked
    fn settle_region(mut self) -> Result<Option<(Results, TypeId, bool)>, Blocked> {
        let settling = self.settling.as_ref().expect("a settle run");
        let region = settling.region;
        let bottom = self.db.bottom();
        let module = self.ir.block(region.entry).func;
        // An annotated variable reads as its annotation, wherever it's read from
        let vars: Box<[VarState]> = self.layout.vars[module.index()]
            .iter()
            .map(|&var| {
                let data = self.ir.var(var);
                match data.annotation {
                    Some(ty) => VarState {
                        ty,
                        may: May::empty(),
                        link: None,
                    },
                    None => VarState {
                        ty: bottom,
                        may: match data.flags.contains(VarFlags::BOTTOM) {
                            true => May::empty(),
                            false => May::UNASSIGNED,
                        },
                        link: None,
                    },
                }
            })
            .collect();
        for &var in &self.layout.vars[module.index()] {
            if let Some(ty) = self.ir.var(var).annotation {
                self.accumulator(var).ty = ty;
            }
        }
        let state = State {
            vars: vars.into_vec(),
            stack: Vec::new(),
            dup: false,
            groups: vec![Group::default(); self.ir.func(module).groups as usize],
        };
        self.merge(region.entry, CtxId::ROOT, state);
        self.iterate();
        if let Some(blocked) = self.blocked() {
            return Err(blocked);
        }
        let mut results = self.final_pass();
        sort(&mut results.problems, &mut results.unresolved);
        let Some(entry) = self.states.get(&(region.exit, CtxId::ROOT)) else {
            return Ok(None);
        };
        let ty = entry.state.vars[self.slot(region.result)].ty;
        Ok(Some((results, ty, self.callable(ty))))
    }

    /// In a settle run, what has blocked it, if anything
    pub(super) fn blocked(&self) -> Option<Blocked> {
        self.settling.as_ref()?.blocked.get()
    }

    /// In a settle run, the value of its region's own method, at its class's
    /// rigids when the method is lifted over them
    pub(super) fn settled_method(&self, func: FuncId, decl: DeclId) -> Option<TypeId> {
        let region = self.settling.as_ref()?.region;
        if region.func != func
            || self
                .tables
                .lifted(decl)
                .is_none_or(|lifted| lifted.is_empty())
            || self.tables.sig_count(decl) != 1
        {
            return None;
        }
        let class = self.db.declaration(region.class).ty;
        let count = match self.db.ty(class) {
            Type::Quantified { binders, .. } => binders.len(),
            _ => 0,
        };
        let (_, rigids) = self
            .scope
            .iter()
            .find(|(owner, _)| *owner == region.class)?;
        let split = self.db.split(self.db.declaration(decl).ty, count);
        Some(self.db.substitute(split, rigids))
    }

    /// In a settle run, note reaching `decl`, a decorated member or accessor, if
    /// it's not settled yet
    pub(super) fn placeholder(&self, decl: DeclId) {
        let Some(settling) = &self.settling else {
            return;
        };
        if self.db.declaration(decl).source.decorated && self.db.settled(decl).is_none() {
            settling.block(Blocked::Member(decl));
        }
    }
}
