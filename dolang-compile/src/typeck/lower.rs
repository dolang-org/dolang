//! Lowering of a unit's elaborated syntax tree into its typing CFG.
//!
//! Lowering follows the bytecode lowerer's shape. A [`Scope`] extends a focused
//! block, switching to another as control flow requires. Statement blocks and
//! function bodies are queued as work, each with the context it's lowered in, and
//! expressions are lowered recursively.
//!
//! A step or terminal pops one operand stack entry per [`ExprKind::Operand`] in it,
//! the most recent entries first, so it must be emitted after everything its
//! operands' entries lie on top of. Where a statement needs its value twice, the
//! value goes in a synthetic variable first, before anything else of the statement
//! is lowered.

mod expr;
mod jump;
mod scope;
mod stmt;
#[cfg(test)]
mod tests;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use jump::{Finally, Loop};
use scope::{DeclKey, Frame};

use super::{
    cfg::{BlockId, Expr, ExprKind, FuncId, FuncKind, Graph, Ir, Origin, Step, Terminal, VarId},
    elab::{DeclAst, ModuleRef, Tables},
    r#type::{Database, DeclId, Literal, SymbolId, UnitId},
};
use crate::{
    ast::{Function, Stmt},
    source::Span,
};

/// Lower `unit` into its typing CFG, one region with the unit's top-level code as
/// its entry function
pub(crate) fn lower(tables: &Tables<'_>, db: &Database, unit: UnitId) -> Ir {
    let lower = Lower::new(tables, db, unit);
    let root = &tables.units[unit.index()]
        .source
        .expect("only a unit with source is lowered")
        .ast
        .0;
    let func = lower.graph.alloc_func(FuncKind::Module(unit), None);
    let frame = lower.frame(func, None, &root.body.vars, &root.body.stmts, None);
    let entry = lower.graph.func(func).entry;
    lower.queue(Work {
        bb: entry,
        ctx: Ctx::function(func, false, frame, None),
        job: Job::Function(root),
    });
    while let Some(work) = lower.pop() {
        lower.run(work);
    }
    lower.finish()
}

struct Lower<'t, 'u> {
    tables: &'t Tables<'u>,
    db: &'t Database,
    unit: UnitId,
    graph: Graph,
    queue: RefCell<Vec<Work<'u>>>,
    decls: HashMap<DeclKey, DeclId>,
    /// The checked modules, by name
    modules: HashMap<&'u str, UnitId>,
    /// The targets each guard block's phantom edges reach, found as closures are
    /// lowered after the statement they're created in
    guards: RefCell<HashMap<BlockId, Vec<BlockId>>>,
    /// Each guard's phantom return: a block that assigns the result `Never` and
    /// continues to the exit
    returns: RefCell<HashMap<BlockId, BlockId>>,
    /// The binder group each type written in the unit is interpreted in, by its span
    site_groups: HashMap<Span, Option<(DeclId, usize)>>,
}

impl<'t, 'u> Lower<'t, 'u> {
    fn new(tables: &'t Tables<'u>, db: &'t Database, unit: UnitId) -> Self {
        let mut decls = HashMap::new();
        for (index, decl) in tables.decls.iter().enumerate() {
            if decl.unit != unit {
                continue;
            }
            let id = DeclId::from_index(index);
            let Some(ast) = &decl.ast else {
                continue;
            };
            match ast {
                DeclAst::Class(class) => {
                    decls.insert(DeclKey::class(class), id);
                }
                DeclAst::Defs(defs) => {
                    decls.extend(defs.iter().map(|def| (DeclKey::def(def), id)));
                }
                DeclAst::Methods(methods) => {
                    decls.extend(methods.iter().map(|method| (DeclKey::method(method), id)));
                }
                DeclAst::Closure(func) => {
                    decls.insert(DeclKey::closure(func), id);
                }
                DeclAst::Alias(_) => {}
            }
        }
        let modules = tables
            .units
            .iter()
            .enumerate()
            .filter_map(|(index, unit)| Some((unit.module?, UnitId::from_index(index))))
            .collect();
        Self {
            tables,
            db,
            unit,
            graph: Graph::new(),
            queue: RefCell::new(Vec::new()),
            decls,
            modules,
            guards: RefCell::new(HashMap::new()),
            returns: RefCell::new(HashMap::new()),
            site_groups: (tables.sites.iter())
                .filter(|site| site.unit == unit)
                .map(|site| (site.ty.span(), site.group()))
                .collect(),
        }
    }

    fn queue(&self, work: Work<'u>) {
        self.queue.borrow_mut().push(work);
    }

    fn pop(&self) -> Option<Work<'u>> {
        self.queue.borrow_mut().pop()
    }

    fn run(&self, work: Work<'u>) {
        let mut scope = Scope {
            lower: self,
            ctx: work.ctx,
            bb: work.bb,
        };
        match work.job {
            Job::Function(func) => scope.function(func),
            Job::Block { stmts, dest, end } => {
                if !scope.stmts(stmts, dest) {
                    scope.finish(end);
                }
            }
        }
    }

    /// Fill in the guards' targets, keep in each function's escapes only the
    /// variables joined where they escape, and freeze the graph. Whether a variable
    /// is volatile is known only now.
    fn finish(self) -> Ir {
        for id in self.graph.func_ids() {
            let mut func = self.graph.func_mut(id);
            func.escapes.retain(|&var| {
                let var = self.graph.var(var);
                !var.volatile && var.origin != Origin::Signature
            });
        }
        for (guard, targets) in self.guards.take() {
            let mut seen = HashSet::new();
            let targets: Vec<_> = targets.into_iter().filter(|&t| seen.insert(t)).collect();
            if let Terminal::Guard { targets: slot, .. } = &mut self.graph.block_mut(guard).terminal
            {
                *slot = targets;
            }
        }
        self.graph.freeze()
    }

    fn decl(&self, key: DeclKey) -> DeclId {
        *self
            .decls
            .get(&key)
            .expect("elaboration collects every declaration of a checked unit")
    }

    fn module(&self, name: &str) -> ModuleRef {
        match self.modules.get(name) {
            Some(&unit) => ModuleRef::Unit(unit),
            None => ModuleRef::External(name.into()),
        }
    }

    fn text(&self, span: Span) -> &'u str {
        self.tables.text(self.unit, span)
    }

    fn symbol(&self, text: &str) -> SymbolId {
        self.db.intern_symbol(text)
    }
}

/// Where lowering is: the function, lexical frame and control context that new
/// blocks and jumps are made in
#[derive(Clone)]
struct Ctx<'u> {
    func: FuncId,
    /// A `do` block, from which `return` leaves the enclosing def
    lambda: bool,
    frame: Rc<Frame<'u>>,
    handler: Option<BlockId>,
    depth: u32,
    /// The innermost `try` with a `finally` whose body or handlers enclose this
    finally: Option<Rc<Finally>>,
    loop_: Option<Loop>,
    /// The guard block of the innermost `NlGuard` statement being lowered
    guard: Option<BlockId>,
    /// The class whose private members are named here
    class: Option<DeclId>,
    /// Lowering a comprehension body's items, whose variable reads and short
    /// circuits go in variables
    hoist: bool,
}

impl<'u> Ctx<'u> {
    fn function(func: FuncId, lambda: bool, frame: Rc<Frame<'u>>, class: Option<DeclId>) -> Self {
        Self {
            func,
            lambda,
            frame,
            handler: None,
            depth: 0,
            finally: None,
            loop_: None,
            guard: None,
            class,
            hoist: false,
        }
    }
}

struct Work<'u> {
    bb: BlockId,
    ctx: Ctx<'u>,
    job: Job<'u>,
}

enum Job<'u> {
    /// A function's parameters and body, starting in its entry block
    Function(&'u Function),
    /// A block's statements, with where its value goes and how it ends
    Block {
        stmts: &'u [Stmt],
        dest: Option<VarId>,
        end: End,
    },
}

/// How a block of statements ends when control reaches its end
enum End {
    Goto {
        target: BlockId,
        /// The `finally` context the target is in
        finally: Option<Rc<Finally>>,
    },
    EndFinally,
}

/// The focused block and the context it's extended in
struct Scope<'l, 't, 'u> {
    lower: &'l Lower<'t, 'u>,
    ctx: Ctx<'u>,
    bb: BlockId,
}

impl<'u> Scope<'_, '_, 'u> {
    fn graph(&self) -> &Graph {
        &self.lower.graph
    }

    /// A new block in the current context
    fn block(&self) -> BlockId {
        self.graph()
            .alloc_block(self.ctx.func, self.ctx.handler, self.ctx.depth)
    }

    fn switch(&mut self, block: BlockId) {
        self.bb = block;
    }

    fn emit(&self, step: Step) {
        self.graph().block_mut(self.bb).steps.push(step);
    }

    fn end(&self, terminal: Terminal) {
        self.graph().block_mut(self.bb).terminal = terminal;
    }

    fn queue(&self, bb: BlockId, ctx: Ctx<'u>, job: Job<'u>) {
        self.lower.queue(Work { bb, ctx, job });
    }

    fn text(&self, span: Span) -> &'u str {
        self.lower.text(span)
    }

    fn symbol(&self, span: Span) -> SymbolId {
        self.lower.symbol(self.text(span))
    }

    /// A variable for a value lowering introduces
    fn synthetic(&self) -> VarId {
        self.graph()
            .alloc_var(self.ctx.func, Origin::Synthetic, None)
    }

    fn assign(&self, var: VarId, value: Expr) {
        self.emit(Step::Assign {
            target: super::cfg::Target::Var(var),
            value,
        });
    }

    fn finish(&mut self, end: End) {
        match end {
            End::Goto { target, finally } => {
                let target = self.route(target, finally.as_ref());
                self.end(Terminal::Branch(target));
            }
            End::EndFinally => self.end(Terminal::EndFinally),
        }
    }
}

fn expr(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

fn nil(span: Span) -> Expr {
    expr(ExprKind::Literal(Literal::Nil), span)
}
