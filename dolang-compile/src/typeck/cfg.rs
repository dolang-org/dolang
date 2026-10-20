//! The semantic typing CFG.
//!
//! A module is lowered to one graph: the module's top-level code is its entry
//! function, and every def, method, lambda and field initializer in it is a
//! function nested in that one. Type flow analyzes the graph in runs, each a
//! function and the nested functions coupled to it ([`Ir::runs`]). A block holds statement
//! steps whose expressions stay tree-shaped, and ends in a terminal. Only
//! short-circuit operators, comprehensions, statements and exceptions introduce
//! control flow.
//!
//! Each function's locals are hoisted to the function, starting unassigned. Only
//! short circuits and comprehensions cross blocks mid-expression. A short
//! circuit's left operand is pushed on an operand stack, and its result is there
//! at the join. The rest of the expression stays a tree, with an
//! [`ExprKind::Operand`] hole where the stack supplies a value. A step or terminal
//! pops one entry per hole, in evaluation order. Below those lie only the pending
//! results of the same statement's earlier short circuits, which a later step
//! pops. [`Terminal::If`] pops its condition, so a short circuit duplicates its
//! left operand first. Statements are never nested in expressions, so an
//! exceptional edge discards the whole stack and enters its handler, an ordinary
//! block, with the exception alone on it.
//!
//! Creating a function is preceded by a [`Step::Capture`] naming it, in the block
//! that creates it. Each function lists in [`Func::escapes`] the variables of its
//! parent that it or a function nested in it captures. Such a function can't run
//! before the step, so the first value of each it can see is the one its owner
//! holds there. Before flow, a pass over each
//! function finds where its variables may have escaped (`Block::escaped`). A
//! function that captures such a variable loads it at its entry, a
//! [`Step::Cache`], and reads and narrows that copy, unless its owner assigns it
//! anywhere it may have escaped ([`VarFlags::UNCACHED`]).
//!
//! A `do` block's unannotated parameters, omitted channels and omitted return type
//! are its [`Signature`]: variables its parent owns and it captures, which start
//! as bottom. The call it's passed to joins what it expects of them into the
//! parameters and channels, and its exit joins its result in, so both functions
//! see each other's changes as they would a capture's.
//!
//! A comprehension's iteratee, conditions and item values are lowered to blocks
//! before the collection or call that holds it, each value assigned to a
//! synthetic variable. Constants, lambdas and collection literals stay in the
//! tree, where the rule's expected type reaches them, with a collection's own
//! items lowered the same way. Items can't assign, so no state crosses iterations:
//! a `for` item is a [`Terminal::Next`] whose body continues to its exit, with no
//! back edge. The collection keeps an [`Item::For`] and [`Item::If`] tree with
//! the variables as its leaves, which says only how often each value occurs.
//! The variables and the comprehension's bindings start as bottom
//! ([`Var::bottom`]), so a path that doesn't assign one adds nothing to it.
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

#[cfg(feature = "debug")]
mod dot;
#[cfg(feature = "debug")]
mod dump;
mod escape;
mod expr;
mod split;
#[cfg(test)]
mod tests;
mod validate;

use std::cell::{Ref, RefCell, RefMut};

use bitvec::boxed::BitBox;
use dolang_util::{alias, mono::MonoVec};

pub(crate) use expr::{Collection, Expr, ExprKind, FmtSpec, Item, Member, Target};
pub(crate) use split::Run;

use super::r#type::{DeclId, MemberKey, SymbolId, TypeId, UnitId};
use crate::{RestKind, source::Span};

macro_rules! id {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(u32);

        $(#[$attr])*
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
id!(GroupId);
id!(RunId);

impl GroupId {
    /// A function's parameters'
    pub(crate) const PARAMS: Self = Self(0);
}

/// A function's source
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncKind {
    /// A module's top-level code
    Module(UnitId),
    /// A def, method implementation, lambda or field initializer
    Decl(DeclId),
    /// The `(init)` a class without one of its own runs: it gives the instance
    /// fields their defaults and passes the instance on
    DefaultInit(DeclId),
}

impl FuncKind {
    /// The declaration whose binders are in scope, its own included
    pub(crate) fn scope(self) -> Option<DeclId> {
        match self {
            Self::Module(_) => None,
            Self::Decl(decl) | Self::DefaultInit(decl) => Some(decl),
        }
    }
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
    /// Bound from the arguments, or in a `do` block, an unannotated item from its
    /// [`Signature`] variable. A parameter's default is a [`Step::Default`] in the
    /// entry block, since it may read captures.
    pub(crate) params: Pattern,
    /// A `do` block's callable state; absent for any other function
    pub(crate) signature: Option<Signature>,
    /// The locals of every scope in the function
    pub(crate) vars: Vec<VarId>,
    /// The variables of enclosing functions it reads or writes
    pub(crate) captures: Vec<VarId>,
    /// The variables its parent owns that it or a function nested in it captures,
    /// leaving out [`Signature`] variables, which are joined as they're assigned
    pub(crate) escapes: Vec<VarId>,
    /// The variables of enclosing functions that it or a function nested in it
    /// reads or writes
    pub(crate) uses: Vec<VarId>,
    /// The variables of enclosing functions that it or a function nested in it
    /// assigns once they're bound
    pub(crate) assigns: Vec<VarId>,
    /// In an `(init)`, the variables standing for the receiver's fields, which a
    /// [`Step::Escape`] checks
    pub(crate) fields: Vec<VarId>,
    /// In an `(init)`, the variables saying whether each superclass with an
    /// `(init)` was initialized, which a [`Step::Escape`] checks
    pub(crate) supers: Vec<VarId>,
    /// How many unpack groups its patterns allocated, [`GroupId::PARAMS`] first
    pub(crate) groups: u32,
}

/// A `do` block's callable state: variables its parent owns and it captures,
/// each absent where the item is annotated or a rest. Its call joins what it expects into
/// the parameters and channels, which the block's entry binds from; its exit
/// joins its result into `result`.
pub(crate) struct Signature {
    /// By parameter item, in order
    pub(crate) params: alias::Box<[Option<VarId>]>,
    pub(crate) input: Option<VarId>,
    pub(crate) output: Option<VarId>,
    pub(crate) result: Option<VarId>,
}

impl Signature {
    /// Its variables
    pub(crate) fn vars(&self) -> impl Iterator<Item = VarId> {
        let vars: Vec<VarId> = (self.params.iter().copied())
            .chain([self.input, self.output, self.result])
            .flatten()
            .collect();
        vars.into_iter()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Source(Span),
    /// Introduced by lowering, such as a `for` loop's iterator or a caught exception
    Synthetic,
    /// The state of one of `self`'s fields in `(init)`
    Field(MemberKey),
    /// Whether `(init)` initialized a superclass of `self`'s class: `true` once
    /// it did
    Super(DeclId),
    /// A function's result
    Result,
    /// Part of a `do` block's [`Signature`]
    Signature,
}

pub(crate) struct Var {
    pub(crate) owner: FuncId,
    pub(crate) origin: Origin,
    /// Closed: the binders of the group it's written in are the rigids its
    /// declaration's body is checked under
    pub(crate) annotation: Option<TypeId>,
    pub(crate) flags: VarFlags,
}

bitflags::bitflags! {
    /// What lowering learned about a [`Var`]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct VarFlags: u8 {
        /// An ivar: read or written by a function other than its owner, which
        /// reads it as an accumulator. Its value when a function that captures it
        /// is created, and every assignment after, joins the accumulator.
        const INTERPROCEDURAL = 1 << 0;
        /// Assigned by a function other than its owner in the unit, so that where
        /// it may have escaped, its owner reads the accumulator too
        const VOLATILE = 1 << 1;
        /// Starts as bottom rather than unassigned: a comprehension's bindings and
        /// item values, which are assigned only on the paths its structure
        /// accounts for
        const BOTTOM = 1 << 2;
        /// Exported from a module, so an importer may assign it anything that fits
        /// its annotation, or anything at all without one, at any time. Such a
        /// variable is also interprocedural; its owner always reads and writes its
        /// accumulator, and no function caches it.
        const EXPORTED = 1 << 3;
        /// Bound by a `def` or `class`, whose name is in scope before its binding
        /// runs, so a function may capture it first. Its binding isn't a fresh
        /// cell.
        const HOISTED = 1 << 4;
        /// Assigned by its owner where it may have escaped, so that the functions
        /// that capture it don't cache it
        const UNCACHED = 1 << 5;
    }
}

pub(crate) struct Block {
    pub(crate) func: FuncId,
    pub(crate) steps: Vec<Step>,
    pub(crate) terminal: Terminal,
    /// Where an exception raised by a step or terminal goes
    pub(crate) handler: Option<BlockId>,
    /// How many `finally` bodies of its function enclose it
    pub(crate) depth: u32,
    /// Its function's variables that may have escaped at its entry, indexed as
    /// [`Func::vars`]; empty until lowering settles escapes
    pub(crate) escaped: BitBox,
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
    Assign(Target),
    /// Join a pattern item's default into its variable's state, since the item may
    /// have been present. It follows the step or edge that binds the pattern.
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
    /// Functions the statement that follows creates, whose [`Func::escapes`] the
    /// accumulators join from here on. It precedes the whole statement, as
    /// nothing a step evaluates assigns or narrows a variable, and can't throw.
    /// A lambda that's a comprehension's item is created in the item's block,
    /// though it stays in the collection's tree.
    Capture(Vec<FuncId>),
    /// Load captures from their accumulators into the function's own state, where
    /// it narrows them. It's the first step of a function's entry block, which
    /// nothing branches to, and names the captures that only their owner assigns,
    /// never where they may have escaped, other than [`Signature`] and result
    /// variables.
    Cache(Vec<VarId>),
    /// In an `(init)`, the receiver becomes observable: each of [`Func::fields`]
    /// but `except` must fit its annotation, and has it from here on. Unless the
    /// receiver goes to a superclass's `(init)`, which expects it partly
    /// initialized, each of [`Func::supers`] must be `true` too.
    Escape {
        span: Span,
        kind: EscapeKind,
        except: Option<VarId>,
        supers: bool,
    },
}

/// How an `(init)`'s receiver becomes observable
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EscapeKind {
    /// It's used other than to access its private fields
    Use,
    /// `(init)` returns it
    Return,
    /// A function capturing it is created
    Capture,
    /// A default `(init)` passes it on
    Default,
}

pub(crate) enum Terminal {
    /// Continue at `target`. A `return`, `break` or `continue` has its
    /// statement's span; a synthetic branch, `Span::INVALID`.
    Branch {
        target: BlockId,
        span: Span,
    },
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
    /// Dispatch the exception to the first clause whose class it is an instance of,
    /// narrowing it there, or else to `otherwise`. The classes pop their operands,
    /// which lie above the exception.
    Catch {
        clauses: alias::Box<[(Expr, BlockId)]>,
        otherwise: BlockId,
    },
    /// Bind the next item to a pattern and continue to the body, or to the exit
    /// when there is none. The item is the `(next)` of the iteratee's `(iter)`,
    /// or with no iteratee, of the strand's ambient input.
    Next {
        iter: Option<VarId>,
        pattern: Pattern,
        body: BlockId,
        exit: BlockId,
        /// The iteratee's
        span: Span,
    },
    /// Return the function's result
    Return,
    Throw(Expr),
    /// A binding failure body returned normally; raises a runtime error.
    ElseFallthrough {
        span: Span,
        kind: usize,
    },
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
        targets: alias::Box<[BlockId]>,
    },
    /// A non-local `break` or `continue`, which continues at the guard point,
    /// with its statement's span
    Escape(Span),
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
    Decl(DeclId),
    /// The class that a class object's type `Type[C]` gives
    Class(Expr),
    /// A value's type, for comparison with a literal
    Value(Expr),
    #[expect(dead_code, reason = "lowering doesn't narrow to a written type yet")]
    Type(TypeId),
    /// A pattern's items, with no variables or constant keys, on its mismatch
    /// edge: a member the pattern always matches is dropped
    Shape(alias::Box<[PatternItem]>),
}

pub(crate) enum Pattern {
    Bind(VarId),
    /// Unpack items. Where the value is a union, the items' types for each of its
    /// alternatives are correlated in `group`, local to the function, which a
    /// later unpack by the same pattern overwrites.
    Unpack {
        items: alias::Box<[PatternItem]>,
        group: GroupId,
    },
}

pub(crate) struct PatternItem {
    pub(crate) key: PatternKey,
    /// Absent for a rest that binds nothing
    pub(crate) var: Option<VarId>,
    /// Whether a default or an optional sub-pattern allows the item to be absent
    pub(crate) default: bool,
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
            params: Pattern::Unpack {
                items: alias::Box::default(),
                group: GroupId::PARAMS,
            },
            signature: None,
            vars: Vec::new(),
            captures: Vec::new(),
            escapes: Vec::new(),
            uses: Vec::new(),
            assigns: Vec::new(),
            fields: Vec::new(),
            supers: Vec::new(),
            groups: 1,
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
            escaped: BitBox::default(),
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
            flags: VarFlags::empty(),
        }));
        self.func_mut(owner).vars.push(id);
        id
    }

    /// Add an unpack group to a function
    pub(crate) fn alloc_group(&self, func: FuncId) -> GroupId {
        let mut func = self.func_mut(func);
        let id = GroupId::from_index(func.groups as usize);
        func.groups = id.0 + 1;
        id
    }

    pub(crate) fn func(&self, id: FuncId) -> Ref<'_, Func> {
        self.funcs[id.index()].borrow()
    }

    pub(crate) fn func_mut(&self, id: FuncId) -> RefMut<'_, Func> {
        self.funcs[id.index()].borrow_mut()
    }

    /// The functions allocated so far
    pub(crate) fn func_ids(&self) -> impl Iterator<Item = FuncId> + use<> {
        (0..self.funcs.len()).map(FuncId::from_index)
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

    /// Finish the graph
    pub(crate) fn freeze(mut self) -> Ir {
        Ir {
            funcs: self.funcs.drain().map(RefCell::into_inner).collect(),
            blocks: self.blocks.drain().map(RefCell::into_inner).collect(),
            vars: self.vars.drain().map(RefCell::into_inner).collect(),
        }
    }
}

/// A finished graph
pub(crate) struct Ir {
    funcs: Vec<Func>,
    blocks: Vec<Block>,
    vars: Vec<Var>,
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
}

impl Terminal {
    /// The blocks of its function it continues to directly. A `Leave`'s tag is
    /// reached through the `EndFinally` that pops it, and a `ReturnFrom` continues
    /// in another function.
    pub(crate) fn successors(&self) -> impl Iterator<Item = BlockId> {
        let mut blocks = Vec::new();
        match self {
            Terminal::Branch { target, .. } => blocks.push(*target),
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
            | Terminal::ElseFallthrough { .. }
            | Terminal::EndFinally
            | Terminal::Escape(_)
            | Terminal::ReturnFrom { .. }
            | Terminal::Unreachable => {}
        }
        blocks.into_iter()
    }

    /// The span of the source construct it lowers, or `Span::INVALID` for a
    /// synthetic one
    pub(crate) fn span(&self) -> Span {
        match self {
            &Terminal::Branch { span, .. } | &Terminal::Escape(span) => span,
            Terminal::If { cond: value, .. }
            | Terminal::Unpack { value, .. }
            | Terminal::Throw(value)
            | Terminal::ReturnFrom { value, .. } => value.span,
            &Terminal::Next { span, .. } => span,
            Terminal::ElseFallthrough { .. }
            | Terminal::Catch { .. }
            | Terminal::Return
            | Terminal::Leave { .. }
            | Terminal::EndFinally
            | Terminal::Guard { .. }
            | Terminal::Unreachable => Span::INVALID,
        }
    }
}
