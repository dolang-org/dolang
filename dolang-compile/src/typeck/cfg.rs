//! The semantic typing CFG.
//!
//! Type flow analyzes one module at a time, as a single region: the module's
//! top-level code is its entry function, and every def, method, lambda and field
//! initializer in it is a function nested in that one. A block holds statement
//! steps whose expressions stay tree-shaped, and ends in a terminal. Only
//! short-circuit operators, statements and exceptions introduce control flow.
//!
//! Each function's locals are hoisted to the function, starting unassigned. Only
//! short circuits cross blocks mid-expression: a short circuit's left operand is
//! pushed on an operand stack, and its result is there at the join. The rest of
//! the expression stays a tree, with an [`ExprKind::Operand`] hole where the stack
//! supplies a value. A step or terminal pops one entry per hole, in evaluation
//! order, and since only earlier short circuits of the same statement lie below,
//! that is the whole stack. [`Terminal::If`] pops its condition, so a short circuit
//! duplicates its left operand first. Statements are never nested in expressions,
//! so an exceptional edge discards the whole stack and enters its handler, an
//! ordinary block, with the exception alone on it.
//!
//! A `finally` is entered by [`Terminal::Leave`] with a tag saying how to continue
//! once [`Terminal::EndFinally`] ends it: at a block, or by rethrowing. Lowering
//! computes each route once, entering an outer `finally` from a trampoline block.
//! During flow, the tags a block was entered with form a stack as deep as the
//! `finally` bodies around it, and a block within a `finally` is analyzed once per
//! stack: the stack is part of the key its state is stored under, not part of the
//! state, so states only merge between equal stacks.
//!
//! A `break`, `continue` or `return` in a `do` block leaves its closure during
//! the call that the closure is an argument of. In the closure, a `break` or
//! `continue` just ends the path ([`Terminal::Escape`]): what it changed in an
//! enclosing function's variables reaches that function through captures. A
//! `return` ([`Terminal::ReturnFrom`]) contributes only its value, as the def's
//! result, to the def's exit block. The rest of the state comes from the enclosing
//! function's guard point, a [`Terminal::Guard`] before the call, whose phantom
//! edges reach each target with the state there. A return's phantom target assigns
//! [`ExprKind::Never`] to the result before continuing to the exit, so the exit
//! joins the returned value with the guard point's state.

mod dump;
mod expr;
#[cfg(test)]
mod tests;
mod validate;

use std::cell::{Cell, Ref, RefCell, RefMut};

use dolang_util::mono::MonoVec;

pub(crate) use expr::{Collection, Expr, ExprKind, FmtSpec, Item, Member, Target};

use super::r#type::{DeclId, MemberKey, SymbolId, TypeId, UnitId};
use crate::{RestKind, source::Span};

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(u32);

        impl $name {
            fn from_index(index: usize) -> Self {
                Self(u32::try_from(index).expect("graph too large"))
            }

            pub(crate) fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id!(FuncId);
id!(BlockId);
id!(VarId);
id!(RuleId);

/// A function's source
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncKind {
    /// A module's top-level code
    Module(UnitId),
    /// A def, method implementation, lambda or field initializer
    Decl(DeclId),
}

pub(crate) struct Func {
    pub(crate) kind: FuncKind,
    pub(crate) parent: Option<FuncId>,
    pub(crate) entry: BlockId,
    /// The one block ending in [`Terminal::Return`]. A return assigns its value to
    /// `result` and continues here, through any `finally`.
    pub(crate) exit: BlockId,
    /// The value it returns, a variable so that it survives a `finally`, which is
    /// entered with an empty stack
    pub(crate) result: VarId,
    /// Bound from the arguments. A parameter's default is a [`Step::Default`] in the
    /// entry block, since it may read captures.
    pub(crate) params: Pattern,
    /// The locals of every scope in the function
    pub(crate) vars: Vec<VarId>,
    /// The variables of enclosing functions it reads or writes
    pub(crate) captures: Vec<VarId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Source(Span),
    /// Introduced by lowering, such as a `for` loop's iterator or a caught exception
    Synthetic,
    /// The state of one of `self`'s fields in `(init)`
    Field(MemberKey),
    /// A function's result
    Result,
}

pub(crate) struct Var {
    pub(crate) owner: FuncId,
    pub(crate) origin: Origin,
    pub(crate) annotation: Option<TypeId>,
    /// Read or written by a nested function
    pub(crate) captured: bool,
    /// Assigned inside a nested function, or after being captured. Such a variable
    /// reads as the join of its assignments inside nested functions, and its owner's
    /// narrowing of it lasts only until a step that can call.
    pub(crate) flagged: bool,
    /// The nested functions that capture it. Filled by [`Graph::freeze`].
    pub(crate) readers: Vec<FuncId>,
}

pub(crate) struct Block {
    pub(crate) func: FuncId,
    pub(crate) steps: Vec<Step>,
    pub(crate) terminal: Terminal,
    /// Where an exception raised by a step or terminal goes
    pub(crate) handler: Option<BlockId>,
    /// How many `finally` bodies of its function enclose it
    pub(crate) depth: u32,
}

/// How a `finally` continues once it ends
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Tag {
    Goto(BlockId),
    /// Raise the pending exception again, through the handler of the block that ends
    /// the `finally`
    Rethrow,
}

pub(crate) enum Step {
    Let {
        pattern: Pattern,
        value: Expr,
    },
    Assign {
        target: Target,
        value: Expr,
    },
    /// Join a parameter's default into its state, since the argument may have been
    /// passed
    Default {
        var: VarId,
        value: Expr,
    },
    Eval(Expr),
    /// Push a short circuit's left operand, or the right operand on its long path
    Push(Expr),
    /// Duplicate the top of the stack, for the condition that [`Terminal::If`] pops
    Dup,
    /// Discard the top of the stack: the left operand on a short circuit's long path
    Pop,
    Assume(Assume),
}

pub(crate) enum Terminal {
    Branch(BlockId),
    /// Test a condition, popping it if it's an [`ExprKind::Operand`]
    If {
        cond: Expr,
        then: BlockId,
        else_: BlockId,
    },
    /// Match a pattern, binding it on the first edge. A shape mismatch takes the
    /// second.
    Unpack {
        pattern: Pattern,
        value: Expr,
        then: BlockId,
        else_: BlockId,
    },
    /// Dispatch the exception on top of the stack to the first clause whose class it
    /// is an instance of, narrowing it there, or else to `otherwise`
    Catch {
        clauses: Vec<(Expr, BlockId)>,
        otherwise: BlockId,
    },
    /// Bind the iterator's next item to a pattern and continue to the body, or to
    /// the exit when it's exhausted
    Next {
        iter: VarId,
        pattern: Pattern,
        body: BlockId,
        exit: BlockId,
    },
    /// Return the function's result
    Return,
    Throw(Expr),
    /// Enter a `finally` at `entry`, pushing a tag
    Leave {
        entry: BlockId,
        tag: Tag,
    },
    /// End a `finally`, popping the tag it was entered with and continuing by it
    EndFinally,
    /// Continue to `next`, and also, with the stack discarded, to each target of a
    /// non-local `break`, `continue` or `return` that a closure instantiated in the
    /// statement may take
    Guard {
        next: BlockId,
        targets: Vec<BlockId>,
    },
    /// A non-local `break` or `continue`, which continues at the guard point
    Escape,
    /// A non-local `return` from an enclosing function, which continues at its exit
    /// block with the value as its result and nothing else known
    ReturnFrom {
        func: FuncId,
        value: Expr,
    },
    Unreachable,
}

/// Narrowing of a variable on one edge of a condition
pub(crate) struct Assume {
    pub(crate) var: VarId,
    pub(crate) relation: Relation,
    pub(crate) negated: bool,
    pub(crate) against: Against,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Relation {
    /// The variable's type is a subtype
    Upper,
    /// The variable's type is exactly it
    Exact,
}

/// What a variable is narrowed against
pub(crate) enum Against {
    /// The class that a class object's type `Type[C]` gives
    Class(Expr),
    /// A value's type, for comparison with a literal
    Value(Expr),
    Type(TypeId),
}

pub(crate) enum Pattern {
    Bind(VarId),
    Unpack(Vec<PatternItem>),
}

pub(crate) struct PatternItem {
    pub(crate) key: PatternKey,
    /// Absent for a rest that binds nothing
    pub(crate) var: Option<VarId>,
    /// Joined into the binding when the item is missing
    pub(crate) default: Option<Expr>,
}

pub(crate) enum PatternKey {
    Pos,
    Key(SymbolId),
    ConstKey(Expr),
    Rest(RestKind),
}

/// A graph under construction. Lowering extends it through shared references, as
/// it holds several parts at once.
#[derive(Default)]
pub(crate) struct Graph {
    funcs: MonoVec<RefCell<Func>>,
    blocks: MonoVec<RefCell<Block>>,
    vars: MonoVec<RefCell<Var>>,
    rules: Cell<u32>,
}

impl Graph {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add a function, with its entry and exit blocks
    pub(crate) fn alloc_func(&self, kind: FuncKind, parent: Option<FuncId>) -> FuncId {
        let id = FuncId::from_index(self.funcs.len());
        if let Some(parent) = parent {
            assert!(parent < id, "a parent function is allocated first");
        }
        let entry = self.alloc_block(id, None, 0);
        let exit = self.alloc_block(id, None, 0);
        self.block_mut(exit).terminal = Terminal::Return;
        let result = VarId::from_index(self.vars.len());
        self.funcs.push(RefCell::new(Func {
            kind,
            parent,
            entry,
            exit,
            result,
            params: Pattern::Unpack(Vec::new()),
            vars: Vec::new(),
            captures: Vec::new(),
        }));
        let allocated = self.alloc_var(id, Origin::Result, None);
        debug_assert_eq!(allocated, result);
        id
    }

    /// Add a block, ending in [`Terminal::Unreachable`] until its terminal is set
    pub(crate) fn alloc_block(
        &self,
        func: FuncId,
        handler: Option<BlockId>,
        depth: u32,
    ) -> BlockId {
        let id = BlockId::from_index(self.blocks.len());
        self.blocks.push(RefCell::new(Block {
            func,
            steps: Vec::new(),
            terminal: Terminal::Unreachable,
            handler,
            depth,
        }));
        id
    }

    pub(crate) fn alloc_var(
        &self,
        owner: FuncId,
        origin: Origin,
        annotation: Option<TypeId>,
    ) -> VarId {
        let id = VarId::from_index(self.vars.len());
        self.vars.push(RefCell::new(Var {
            owner,
            origin,
            annotation,
            captured: false,
            flagged: false,
            readers: Vec::new(),
        }));
        self.func_mut(owner).vars.push(id);
        id
    }

    pub(crate) fn alloc_rule(&self) -> RuleId {
        let id = RuleId(self.rules.get());
        self.rules
            .set(id.0.checked_add(1).expect("graph too large"));
        id
    }

    pub(crate) fn func(&self, id: FuncId) -> Ref<'_, Func> {
        self.funcs[id.index()].borrow()
    }

    pub(crate) fn func_mut(&self, id: FuncId) -> RefMut<'_, Func> {
        self.funcs[id.index()].borrow_mut()
    }

    pub(crate) fn block(&self, id: BlockId) -> Ref<'_, Block> {
        self.blocks[id.index()].borrow()
    }

    pub(crate) fn block_mut(&self, id: BlockId) -> RefMut<'_, Block> {
        self.blocks[id.index()].borrow_mut()
    }

    pub(crate) fn var(&self, id: VarId) -> Ref<'_, Var> {
        self.vars[id.index()].borrow()
    }

    pub(crate) fn var_mut(&self, id: VarId) -> RefMut<'_, Var> {
        self.vars[id.index()].borrow_mut()
    }

    /// Finish the graph, recording each captured variable's readers
    pub(crate) fn freeze(mut self) -> Ir {
        let funcs: Vec<Func> = self.funcs.drain().map(RefCell::into_inner).collect();
        let mut vars: Vec<Var> = self.vars.drain().map(RefCell::into_inner).collect();
        for (index, func) in funcs.iter().enumerate() {
            for &var in &func.captures {
                vars[var.index()].readers.push(FuncId::from_index(index));
            }
        }
        Ir {
            funcs,
            blocks: self.blocks.drain().map(RefCell::into_inner).collect(),
            vars,
            rules: self.rules.get(),
        }
    }
}

/// A finished graph
pub(crate) struct Ir {
    funcs: Vec<Func>,
    blocks: Vec<Block>,
    vars: Vec<Var>,
    rules: u32,
}

impl Ir {
    pub(crate) fn func(&self, id: FuncId) -> &Func {
        &self.funcs[id.index()]
    }

    pub(crate) fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.index()]
    }

    pub(crate) fn var(&self, id: VarId) -> &Var {
        &self.vars[id.index()]
    }

    pub(crate) fn funcs(&self) -> impl Iterator<Item = (FuncId, &Func)> {
        (self.funcs.iter().enumerate()).map(|(index, func)| (FuncId::from_index(index), func))
    }

    pub(crate) fn blocks(&self) -> impl Iterator<Item = (BlockId, &Block)> {
        (self.blocks.iter().enumerate()).map(|(index, block)| (BlockId::from_index(index), block))
    }

    /// How many rules were allocated
    pub(crate) fn rules(&self) -> usize {
        self.rules as usize
    }
}

impl Terminal {
    /// The blocks of its function it continues to directly. A `Leave`'s tag is
    /// reached through the `EndFinally` that pops it, and a `ReturnFrom` continues
    /// in another function.
    pub(crate) fn successors(&self) -> impl Iterator<Item = BlockId> {
        let mut blocks = Vec::new();
        match self {
            Terminal::Branch(block) => blocks.push(*block),
            Terminal::If { then, else_, .. } | Terminal::Unpack { then, else_, .. } => {
                blocks.extend([*then, *else_]);
            }
            Terminal::Catch { clauses, otherwise } => {
                blocks.extend(clauses.iter().map(|&(_, block)| block));
                blocks.push(*otherwise);
            }
            Terminal::Next { body, exit, .. } => blocks.extend([*body, *exit]),
            Terminal::Leave { entry, .. } => blocks.push(*entry),
            Terminal::Guard { next, targets } => {
                blocks.push(*next);
                blocks.extend(targets);
            }
            Terminal::Return
            | Terminal::Throw(_)
            | Terminal::EndFinally
            | Terminal::Escape
            | Terminal::ReturnFrom { .. }
            | Terminal::Unreachable => {}
        }
        blocks.into_iter()
    }
}
