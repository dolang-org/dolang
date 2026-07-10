use std::{
    cell::{Ref, RefCell, RefMut},
    collections::HashSet,
};

use super::{ast::Var, constant, origin, sig, source::Span, sym};
use dolang_util::arena::ArenaVec;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeId(usize);

pub(crate) struct Scope {
    pub(crate) parent: Option<ScopeId>,
    pub(crate) local_offset: usize,
    pub(crate) vars: Vec<Var>,
    pub(crate) caps: usize,
    pub(crate) func_upvar_depth: usize,
    pub(crate) blocks: ArenaVec<BlockId>,
    pub(crate) is_nl_guard: bool,
    pub(crate) func: Option<FuncId>,
}

impl Scope {
    /// Whether this scope has an upvar record at runtime.
    ///
    /// Returns true if it has captures or is an NL guard (which always needs an upvar record).
    pub(crate) fn has_upvars(&self) -> bool {
        self.caps != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub(crate) struct BlockId(usize);

impl BlockId {
    pub(crate) fn index(&self) -> usize {
        self.0
    }
}

pub(crate) enum InstInfo {
    Add,
    Call(sig::PackId),
    MethodCall(sym::Id, sig::PackId),
    Builtin(usize, sig::PackId),
    Div,
    Ediv,
    Dup,
    Swap(usize, usize),
    LoadConst(constant::Id),
    LoadLocal(usize),
    LoadUpvar(usize, usize),
    Get(sym::Id),
    Set(sym::Id),
    Index,
    Assign,
    Mod,
    Mul,
    Neg,
    Not,
    BitNot,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    #[expect(dead_code)]
    Nop,
    Pop,
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
    StoreLocal(usize),
    StoreUpvar(usize, usize),
    Sub,
    PushUpvars(usize),
    PopUpvars,
    Close(FuncId),
    Reify(sig::PackId),
    Next,
    Unpack(sig::UnpackId),
    NlGuard(FuncId),
}

pub(crate) struct Inst(pub(crate) InstInfo, pub(crate) Span);

#[derive(Default)]
pub(crate) enum TermInfo {
    #[default]
    Ret,
    Branch(BlockId),
    If(BlockId, BlockId),
    NlBranch(usize, u8),
}

#[derive(Default)]
pub(crate) struct Term(pub(crate) TermInfo, pub(crate) Span);

pub(crate) struct Block {
    pub(crate) insts: Vec<Inst>,
    pub(crate) term: Term,
    pub(crate) inbound: HashSet<BlockId>,
    pub(crate) scope: ScopeId,
    pub(crate) func: FuncId,
}

pub(crate) type ScopeRef<'c> = Ref<'c, Scope>;

pub(crate) struct Func {
    pub(crate) enter: BlockId,
    pub(crate) exit: BlockId,
    pub(crate) sig: sig::UnpackId,
    pub(crate) locals: usize,
    pub(crate) scopes: ArenaVec<ScopeId>,
    pub(crate) name: Option<Span>,
    pub(crate) class_name: Option<Span>,
}

pub(crate) type BlockRef<'c> = Ref<'c, Block>;
pub(crate) type BlockRefMut<'c> = RefMut<'c, Block>;

impl Func {
    fn new(sig: sig::UnpackId, name: Option<Span>) -> Self {
        Self {
            enter: BlockId(0),
            exit: BlockId(0),
            sig,
            locals: 0,
            scopes: Default::default(),
            name,
            class_name: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FuncId(usize);

impl FuncId {
    pub(crate) fn index(&self) -> usize {
        self.0
    }
}

pub(crate) type FuncRef<'c> = Ref<'c, Func>;
pub(crate) type FuncRefMut<'c> = RefMut<'c, Func>;

pub(crate) struct Graph {
    funcs: ArenaVec<RefCell<Func>>,
    scopes: ArenaVec<RefCell<Scope>>,
    blocks: ArenaVec<RefCell<Block>>,
}

impl Graph {
    pub(crate) fn new() -> Self {
        Self {
            funcs: Default::default(),
            scopes: Default::default(),
            blocks: Default::default(),
        }
    }

    pub(crate) fn alloc_scope(
        &self,
        is_func: bool,
        is_nl_guard: bool,
        func: FuncId,
        parent: Option<ScopeId>,
        vars: &[Var],
        origintab: &origin::Table,
    ) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        let mut borrow = self.func_mut(func);
        let caps: usize = vars.iter().map(|l| l.captured as usize).sum();
        let locals = vars
            .iter()
            .map(|v| v.is_emitted(origintab) as usize)
            .sum::<usize>()
            - caps;
        let local_offset = if is_func {
            borrow.locals = locals;
            0
        } else {
            let parent = self.scope(parent.unwrap());
            let offset = parent.local_offset.strict_add(
                parent
                    .vars
                    .iter()
                    .map(|v| v.is_emitted(origintab) as usize)
                    .sum::<usize>()
                    - parent.caps,
            );
            borrow.locals = borrow.locals.max(offset.strict_add(locals));
            offset
        };
        let func_upvar_depth = if !is_func && let Some(parent) = parent {
            self.scope(parent).func_upvar_depth
        } else {
            0
        };
        let has_upvars = caps != 0;
        let func_upvar_depth = func_upvar_depth + has_upvars as usize;
        let scope = Scope {
            parent,
            local_offset,
            caps,
            func_upvar_depth,
            blocks: Default::default(),
            vars: vars.into(),
            is_nl_guard,
            func: if is_func { Some(func) } else { None },
        };
        borrow.scopes.push(id);
        self.scopes.push(RefCell::new(scope));
        id
    }

    pub(crate) fn scope<'s>(&'s self, id: ScopeId) -> ScopeRef<'s> {
        self.scopes[id.0].borrow()
    }

    pub(crate) fn alloc_block(&self, func: FuncId, scope: ScopeId) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(RefCell::new(Block {
            scope,
            func,
            insts: Default::default(),
            term: Default::default(),
            inbound: Default::default(),
        }));
        self.scope(scope).blocks.push(id);
        id
    }

    pub(crate) fn block_mut<'s>(&'s self, id: BlockId) -> BlockRefMut<'s> {
        self.blocks[id.0].borrow_mut()
    }

    pub(crate) fn block<'s>(&'s self, id: BlockId) -> BlockRef<'s> {
        self.blocks[id.0].borrow()
    }

    /// Allocate a synthetic NL guard function (zero-arg, with forced upvar record)
    pub(crate) fn alloc_nl_guard(
        &self,
        sig: sig::UnpackId,
        scope: Option<ScopeId>,
        origintab: &origin::Table,
    ) -> FuncId {
        let id = FuncId(self.funcs.len());
        self.funcs.push(RefCell::new(Func::new(sig, None)));
        let sid = self.alloc_scope(true, true, id, scope, &[], origintab);
        let enter = self.alloc_block(id, sid);
        let exit = self.alloc_block(id, sid);
        let mut func = self.funcs[id.index()].borrow_mut();
        func.enter = enter;
        func.exit = exit;
        id
    }

    pub(crate) fn alloc_func(
        &self,
        sig: sig::UnpackId,
        name: Option<Span>,
        vars: &[Var],
        scope: Option<ScopeId>,
        origintab: &origin::Table,
    ) -> FuncId {
        let id = FuncId(self.funcs.len());
        self.funcs.push(RefCell::new(Func::new(sig, name)));
        let sid = self.alloc_scope(true, false, id, scope, vars, origintab);
        let enter = self.alloc_block(id, sid);
        let exit = self.alloc_block(id, sid);
        let mut func = self.funcs[id.index()].borrow_mut();
        func.enter = enter;
        func.exit = exit;
        id
    }

    pub(crate) fn func_mut<'s>(&'s self, id: FuncId) -> FuncRefMut<'s> {
        self.funcs[id.0].borrow_mut()
    }

    pub(crate) fn func<'s>(&'s self, id: FuncId) -> FuncRef<'s> {
        self.funcs[id.0].borrow()
    }

    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn func_count(&self) -> usize {
        self.funcs.len()
    }

    #[expect(dead_code)]
    pub(crate) fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    pub(crate) fn iter_funcs(&self) -> impl Iterator<Item = FuncId> + 'static {
        (0..self.funcs.len()).map(FuncId)
    }

    #[allow(dead_code)]
    pub(crate) fn iter_scopes(&self) -> impl Iterator<Item = ScopeId> + 'static {
        (0..self.scopes.len()).map(ScopeId)
    }

    pub(crate) fn iter_blocks(&self) -> impl Iterator<Item = BlockId> + 'static {
        (0..self.blocks.len()).map(BlockId)
    }
}

#[cfg(feature = "debug")]
pub(crate) mod dot;
