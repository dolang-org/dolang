//! Which variables are assigned at only one site, decided before flow.
//!
//! A site is syntactic: a binding or assignment step, a pattern a terminal
//! binds, a function's parameters, or a parameter's default. A site in a loop,
//! or in a function called many times, is still one site. Every function of
//! the unit counts, so an assignment in a closure that captures a variable is
//! a site of that variable. A variable with exactly one is [`VarFlags::ONCE`].

use rustc_hash::FxHashMap;

use super::{Ir, Step, Target, Terminal, VarFlags, VarId};

impl Ir {
    /// Mark the variables assigned at exactly one site [`VarFlags::ONCE`]
    pub(crate) fn settle_assignments(&mut self) {
        let mut sites: FxHashMap<VarId, u32> = FxHashMap::default();
        let mut site = |var: VarId| *sites.entry(var).or_default() += 1;
        for func in &self.funcs {
            func.params.vars().for_each(&mut site);
        }
        for block in &self.blocks {
            for step in &block.steps {
                match step {
                    Step::Let { pattern, .. } => pattern.vars().for_each(&mut site),
                    Step::Assign(Target::Var { var, .. }) | Step::Default { var, .. } => {
                        site(*var);
                    }
                    _ => {}
                }
            }
            match &block.terminal {
                Terminal::Unpack { pattern, .. } | Terminal::Next { pattern, .. } => {
                    pattern.vars().for_each(&mut site);
                }
                _ => {}
            }
        }
        for (var, count) in sites {
            if count == 1 {
                self.vars[var.index()].flags.insert(VarFlags::ONCE);
            }
        }
    }
}
