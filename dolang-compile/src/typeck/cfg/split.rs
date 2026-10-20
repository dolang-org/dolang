//! How a unit's flow divides into runs, decided before flow.
//!
//! A run covers a root function and the descendants it includes. Each child of
//! an included function is classified by the variables the run's functions own,
//! leaving out exported ones, which couple nothing:
//!
//! - **Included** if it's a `do` block, whose [`Signature`](super::Signature)
//!   couples it to its caller, or if it or a function nested in it assigns such a
//!   variable ([`Func::assigns`](super::Func::assigns)).
//! - **Detached late** if it or a function nested in it captures one
//!   ([`Func::uses`](super::Func::uses)): its run starts once this one finishes, when their
//!   accumulators are final.
//! - **Detached early** otherwise: its run may start before this one.
//!
//! A detached child roots a run of its own. Every function that assigns a
//! variable is in its owner's run, so what a run reads from the runs above it,
//! [`Run::inherited`], is fixed before it starts.

use super::{FuncId, Ir, RunId, VarFlags, VarId};

/// Trace under `typeck.split`
macro_rules! trace {
    ($($arg:tt)*) => {
        dolang_util::debug_eprintln!(topic: "typeck.split", $($arg)*)
    };
}

/// A flow run: a root function and the descendants it includes
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) root: FuncId,
    /// The root and the functions it includes, in ID order
    pub(crate) funcs: Box<[FuncId]>,
    /// The runs detached early, which may start before this one
    pub(crate) early: Box<[RunId]>,
    /// The runs detached late, which start once this one finishes
    pub(crate) late: Box<[RunId]>,
    /// What it reads from the runs above it: its root's non-exported uses
    pub(crate) inherited: Box<[VarId]>,
}

impl RunId {
    /// The run rooted at the module function, which every other is detached from
    pub(crate) fn module() -> Self {
        Self::from_index(0)
    }
}

impl Ir {
    /// The runs its flow divides into, by ID, in preorder: the module function's
    /// first, and each run's detached runs after it, in order of their roots
    pub(crate) fn runs(&self) -> Box<[Run]> {
        let mut children = vec![Vec::new(); self.funcs.len()];
        for (id, func) in self.funcs() {
            if let Some(parent) = func.parent {
                children[parent.index()].push(id);
            }
        }
        let mut split = Split {
            ir: self,
            children,
            run_of: vec![None; self.funcs.len()],
            runs: Vec::new(),
        };
        split.run(FuncId::from_index(0));
        split.runs.into()
    }
}

struct Split<'a> {
    ir: &'a Ir,
    /// Each function's children, in ID order
    children: Vec<Vec<FuncId>>,
    /// The run each function classified so far is in
    run_of: Vec<Option<RunId>>,
    runs: Vec<Run>,
}

impl Split<'_> {
    /// Add the run rooted at `root`, then its detached runs
    fn run(&mut self, root: FuncId) -> RunId {
        let ir = self.ir;
        let id = RunId::from_index(self.runs.len());
        let inherited = (ir.func(root).uses.iter().copied())
            .filter(|&var| !ir.var(var).flags.contains(VarFlags::EXPORTED))
            .collect();
        self.runs.push(Run {
            root,
            funcs: Box::default(),
            early: Box::default(),
            late: Box::default(),
            inherited,
        });
        self.run_of[root.index()] = Some(id);
        let mut funcs = vec![root];
        let mut detached = Vec::new();
        let mut next = 0;
        while let Some(&func) = funcs.get(next) {
            next += 1;
            for &child in &self.children[func.index()] {
                let data = ir.func(child);
                if data.signature.is_some() || self.owned(id, &data.assigns) {
                    self.run_of[child.index()] = Some(id);
                    funcs.push(child);
                } else {
                    detached.push((child, self.owned(id, &data.uses)));
                }
            }
        }
        funcs.sort();
        detached.sort();
        let (mut early, mut late) = (Vec::new(), Vec::new());
        for (child, after) in detached {
            let run = self.run(child);
            match after {
                true => late.push(run),
                false => early.push(run),
            }
        }
        trace!(
            "run {}: root {:?}, {} functions, early {:?}, late {:?}, inherits {:?}",
            id.index(),
            root,
            funcs.len(),
            early.iter().map(|run| run.index()).collect::<Vec<_>>(),
            late.iter().map(|run| run.index()).collect::<Vec<_>>(),
            self.runs[id.index()].inherited,
        );
        let run = &mut self.runs[id.index()];
        run.funcs = funcs.into();
        run.early = early.into();
        run.late = late.into();
        id
    }

    /// Whether any of `vars` is owned in the run and not exported
    fn owned(&self, run: RunId, vars: &[VarId]) -> bool {
        vars.iter().any(|&var| {
            let var = self.ir.var(var);
            self.run_of[var.owner.index()] == Some(run) && !var.flags.contains(VarFlags::EXPORTED)
        })
    }
}
