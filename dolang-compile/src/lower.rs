use std::{cell::OnceCell, mem, str::Utf8Error};

use dolang_util::arena::ArenaVec;

use dolang_bytecode::builtin;

use crate::{
    Mode, PreludeImport,
    ast::{
        Arg, ArrayElem, Assign, Bind, Block, Class, Const, Decorator, Def, DefVariant, DictElem,
        Expand, Expr, For, Function, GetVariant, Ident, If, Import, ImportElement, ImportItem, Key,
        LValue, Let, NlGuard, Pair, Param, ParamDefault, Pattern, PrimStmt, Res, Return, Single,
        Stmt, Try, Unit, While, visit::Node,
    },
    cfg::{self, BlockRefMut, Inst, InstInfo, Term, TermInfo},
    constant::{self, ConstantExt},
    intern,
    lex::Op,
    origin, sig,
    source::{File, Span},
    sym,
};

pub(crate) struct Lowerer<'c> {
    pub(crate) mode: Mode<'c>,
    pub(crate) file: &'c File<'c>,
    pub(crate) symtab: &'c mut sym::Table,
    pub(crate) bintab: &'c mut intern::BinTable,
    pub(crate) consttab: &'c mut constant::Table,
    pub(crate) packtab: &'c mut sig::PackTable,
    pub(crate) unpacktab: &'c mut sig::UnpackTable,
    pub(crate) origintab: &'c origin::Table,
    pub(crate) prelude: &'c [PreludeImport],
    pub(crate) sentinel_const: Option<constant::Id>,
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct Error {}

impl From<Utf8Error> for Error {
    fn from(_value: Utf8Error) -> Self {
        Self {}
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

enum Var {
    Local(usize),
    Upvar(usize, usize),
}

struct Params<'a> {
    bind: Option<Vec<Var>>,
    bind_params: Option<&'a [Param]>,
    mode: Mode<'a>,
    unpack: Option<sig::UnpackId>,
    is_top_level: bool,
    next_id: Option<cfg::BlockId>,
    break_id: Option<cfg::BlockId>,
    break_result: bool,
    continue_id: Option<cfg::BlockId>,
    exit_id: cfg::BlockId,
}

enum WorkAst<'a> {
    Function(&'a Function, sig::UnpackId),
    ClassBody(&'a Block),
    Block(&'a Block, bool),
    Stmt(&'a Stmt),
    Args(&'a [Arg]),
    ArrayElems(&'a [ArrayElem]),
    DictElems(&'a [DictElem]),
}

struct Work<'a> {
    ast: WorkAst<'a>,
    bb: cfg::BlockId,
    params: Params<'a>,
}

type Queue<'a> = ArenaVec<Work<'a>>;

struct Scope<'a, 'c, 'q> {
    file: &'c File<'c>,
    symtab: &'c mut sym::Table,
    bintab: &'c mut intern::BinTable,
    consttab: &'c mut constant::Table,
    packtab: &'c mut sig::PackTable,
    unpacktab: &'c mut sig::UnpackTable,
    origintab: &'c origin::Table,
    prelude: &'c [PreludeImport],
    sentinel_const: &'c mut Option<constant::Id>,
    graph: &'a cfg::Graph,
    bb: cfg::BlockId,
    params: Params<'a>,
    block: BlockRefMut<'a>,
    queue: &'q Queue<'a>,
}

impl<'a, 'c, 'q> Scope<'a, 'c, 'q> {
    fn queue(&self, work: Work<'a>) {
        self.queue.push(work)
    }

    fn switch(&mut self, id: cfg::BlockId) {
        if self.bb != id {
            self.bb = id;
            self.block = self.graph.block_mut(id);
        }
    }

    fn link(&mut self, id: cfg::BlockId) {
        if id == self.bb {
            self.block.inbound.insert(id);
        } else {
            self.graph.block_mut(id).inbound.insert(self.bb);
        }
    }

    fn lower_logical_and(&mut self, left: &'a Expr, right: &'a Expr, span: Span) -> Result<()> {
        let tid = self.graph.alloc_block(self.block.func, self.block.scope);
        let next = self.graph.alloc_block(self.block.func, self.block.scope);
        self.lower_expr(left)?;
        self.block.insts.push(Inst(InstInfo::Dup, span));
        self.block.term = Term(TermInfo::If(tid, next), span);
        self.link(tid);
        self.link(next);
        self.switch(tid);
        self.block.insts.push(Inst(InstInfo::Pop, span));
        self.lower_expr(right)?;
        self.block.term = Term(TermInfo::Branch(next), span);
        self.link(next);
        self.switch(next);
        Ok(())
    }

    fn lower_logical_or(&mut self, left: &'a Expr, right: &'a Expr, span: Span) -> Result<()> {
        let fid = self.graph.alloc_block(self.block.func, self.block.scope);
        let next = self.graph.alloc_block(self.block.func, self.block.scope);
        self.lower_expr(left)?;
        self.block.insts.push(Inst(InstInfo::Dup, span));
        self.block.term = Term(TermInfo::If(next, fid), span);
        self.link(fid);
        self.link(next);
        self.switch(fid);
        self.block.insts.push(Inst(InstInfo::Pop, span));
        self.lower_expr(right)?;
        self.block.term = Term(TermInfo::Branch(next), span);
        self.link(next);
        self.switch(next);
        Ok(())
    }

    /// # Variable Resolution Algorithm
    ///
    /// This function maps a compile-time variable reference (by index and depth) to its runtime
    /// representation (local variable or upvar).
    ///
    /// ## Depth Calculation
    ///
    /// The `depth` parameter is the lexical scope depth from the resolver. We need to convert this
    /// to an upvar depth for the runtime.
    ///
    /// The algorithm walks up the scope chain:
    /// - Each scope with upvars increments the upvar count
    /// - NL guard scopes are synthetic and *not* counted in the resolver's depth,
    ///   but they *do* create an extra upvar frame at runtime for the non-local jump
    ///   mechanism
    ///
    /// ## NL Guard Scope Handling
    ///
    /// NL guard scopes are created during lowering to implement `break`/`continue`/
    /// `return` across closure boundaries. They:
    /// - Are not present in the source-level scope tree
    /// - Add an extra upvar frame at runtime
    /// - Must be accounted for when calculating upvar depth but not when counting
    ///   lexical scope depth
    ///
    /// ## Local vs Upvar
    ///
    /// Once we reach the target scope:
    /// - If the variable is captured, it's an upvar (index into upvar record chain)
    /// - Otherwise, it's a local (index into local variable array)
    fn resolve_var_in_scope(
        &self,
        mut scope: cfg::ScopeRef<'a>,
        index: usize,
        depth: usize,
    ) -> Var {
        let mut up = 0;
        let mut remaining = depth;
        // Walk up the scope chain, counting upvar frames
        while remaining > 0 {
            up += (scope.has_upvars()) as usize;
            // NL guard scopes are synthetic and not counted by the resolver,
            // but they add an extra upvar frame at runtime for the jump target
            if !scope.is_nl_guard {
                remaining -= 1;
            } else {
                up += 1;
            }
            scope = self
                .graph
                .scope(scope.parent.expect("var depth exceeds scope depth"));
        }
        // If we stopped at an NL guard scope, skip past it to the real scope
        if scope.is_nl_guard {
            up += 1;
            scope = self
                .graph
                .scope(scope.parent.expect("var depth exceeds scope depth"));
        }
        // Find the variable in the target scope
        let mut caps = 0;
        for (i, (j, local)) in scope
            .vars
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_emitted(self.origintab))
            .enumerate()
        {
            if index == j {
                return if local.captured {
                    Var::Upvar(caps, up)
                } else {
                    Var::Local(i.strict_sub(caps).strict_add(scope.local_offset))
                };
            }
            caps += local.captured as usize
        }
        unreachable!()
    }

    fn resolve_var(&self, index: usize, depth: usize) -> Var {
        self.resolve_var_in_scope(self.graph.scope(self.block.scope), index, depth)
    }

    // The logic here is similar to the resolution algorithm above, but we only care about upvar
    // depth (for NL branch target calculation) and not resolving a particular variable
    fn scope_to_upvar_depth(&self, scope_depth: usize) -> usize {
        let mut scope = self.graph.scope(self.block.scope);
        let mut upvar_depth = 0;
        for _ in 0..scope_depth {
            upvar_depth += scope.has_upvars() as usize;
            if scope.is_nl_guard {
                upvar_depth += 1;
            }
            scope = self.graph.scope(scope.parent.unwrap());
        }
        upvar_depth
    }

    fn lower_store_res(&mut self, res: &Res, span: Span, want_result: bool) {
        let var = self.resolve_var(res.index, res.depth);
        if want_result {
            self.block.insts.push(Inst(InstInfo::Dup, span));
        }
        self.lower_store(span, var);
    }

    fn lower_store(&mut self, span: Span, var: Var) {
        match var {
            Var::Local(index) => self
                .block
                .insts
                .push(Inst(InstInfo::StoreLocal(index), span)),
            Var::Upvar(index, depth) => self
                .block
                .insts
                .push(Inst(InstInfo::StoreUpvar(index, depth), span)),
        }
    }

    fn lower_load(&mut self, res: &Res, span: Span) {
        let var = self.resolve_var(res.index, res.depth);
        match var {
            Var::Local(index) => self
                .block
                .insts
                .push(Inst(InstInfo::LoadLocal(index), span)),
            Var::Upvar(index, depth) => self
                .block
                .insts
                .push(Inst(InstInfo::LoadUpvar(index, depth), span)),
        }
    }

    fn lower_concat(
        &mut self,
        exprs: &'a [Expr],
        span: Option<Span>,
        external: bool,
    ) -> Result<()> {
        let mut acc = String::new();
        let span = span.unwrap_or_else(|| exprs[0].span() | exprs.last().unwrap().span());
        let mut concat = 0;

        if exprs.is_empty() {
            let cid = self.consttab.str(self.bintab.id_str(&acc));
            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
            return Ok(());
        }

        for expr in exprs.iter() {
            match expr {
                Expr::Literal(span) => {
                    acc.push_str(self.file.str(*span));
                }
                Expr::Escape(char, _) => {
                    acc.push(*char);
                }
                other => {
                    if !acc.is_empty() {
                        let cid = self.consttab.str(self.bintab.id_str(&acc));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        acc.clear();
                        concat += 1;
                    }
                    self.lower_expr(other)?;
                    concat += 1;
                }
            }
        }

        if !acc.is_empty() {
            let cid = self.consttab.str(self.bintab.id_str(&acc));
            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
            if concat != 0 {
                concat += 1
            }
        }

        if concat != 0 {
            let sig = sig::Pack::new(vec![sig::Arg::Value; concat].into_iter());
            self.block.insts.push(Inst(
                InstInfo::Builtin(
                    if external {
                        builtin::CONCAT_ARG
                    } else {
                        builtin::CONCAT_STR
                    },
                    self.packtab.id(&sig),
                ),
                span,
            ));
        }
        Ok(())
    }

    fn lower_bin_concat(&mut self, exprs: &'a [Expr], span: Span) -> Result<()> {
        let mut acc: Vec<u8> = Vec::new();
        let mut count = 0usize;

        if exprs.is_empty() {
            let cid = self.consttab.bin(self.bintab.id(&acc));
            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
            return Ok(());
        }

        for expr in exprs.iter() {
            match expr {
                Expr::Literal(lspan) => {
                    acc.extend_from_slice(self.file.str(*lspan).as_bytes());
                }
                Expr::EscapeByte(b, _) => {
                    acc.push(*b);
                }
                other => {
                    if !acc.is_empty() {
                        let cid = self.consttab.bin(self.bintab.id(&acc));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        acc.clear();
                        count += 1;
                    }
                    self.lower_expr(other)?;
                    count += 1;
                }
            }
        }

        if !acc.is_empty() {
            let cid = self.consttab.bin(self.bintab.id(&acc));
            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
            if count != 0 {
                count += 1;
            }
        }

        if count != 0 {
            let sig = sig::Pack::new(vec![sig::Arg::Value; count].into_iter());
            self.block.insts.push(Inst(
                InstInfo::Builtin(builtin::CONCAT_BIN, self.packtab.id(&sig)),
                span,
            ));
        }
        Ok(())
    }

    fn lower_expr(&mut self, expr: &'a Expr) -> Result<()> {
        match expr {
            Expr::Error => unreachable!(),
            Expr::Literal(span) => {
                let cid = self.consttab.str(self.bintab.id_str(self.file.str(*span)));
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Concat {
                exprs,
                delim_span,
                arg: external,
            } => self.lower_concat(exprs, *delim_span, *external)?,
            Expr::Escape(char, span) => {
                let cid = self.consttab.str(self.bintab.id_str(&format!("{char}")));
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::BinConcat { exprs, open, close } => {
                let span = *open | *close;
                self.lower_bin_concat(exprs, span)?;
            }
            Expr::EscapeByte(b, span) => {
                let bytes = [*b];
                let cid = self.consttab.bin(self.bintab.id(&bytes));
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Ident(ident) => {
                let res = ident.res.as_ref().unwrap();
                match self.resolve_var(res.index, res.depth) {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::LoadLocal(index), ident.span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::LoadUpvar(index, depth), ident.span)),
                }
            }
            Expr::Int(v, span) => {
                let cid = self.consttab.int(*v);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::VerbatimInt(v, span) => {
                let id = self.bintab.id_str(self.file.str(*span));
                let cid = self.consttab.verbatim_int(*v, id);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::F64(v, span) => {
                let cid = self.consttab.f64(*v);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::VerbatimF64(v, span) => {
                let id = self.bintab.id_str(self.file.str(*span));
                let cid = self.consttab.verbatim_f64(*v, id);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Bool(v, span) => {
                let cid = self.consttab.bool(*v);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Nil(span) => {
                let cid = self.consttab.nil();
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Sym(span) => {
                let id = self.symtab.id(&self.bintab.id_str(self.file.str(*span)));
                let cid = self.consttab.sym(id);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::Group { expr, .. } => self.lower_expr(expr)?,
            Expr::Unary { op, expr, op_span } => {
                self.lower_expr(expr)?;
                self.block.insts.push(Inst(
                    match op {
                        Op::Minus => InstInfo::Neg,
                        Op::Bang => InstInfo::Not,
                        Op::Tilde => InstInfo::BitNot,
                        _ => unreachable!(),
                    },
                    *op_span,
                ));
            }
            Expr::Binary { op, exprs, op_span } => {
                match op {
                    Op::AmpAmp => return self.lower_logical_and(&exprs[0], &exprs[1], *op_span),
                    Op::BarBar => return self.lower_logical_or(&exprs[0], &exprs[1], *op_span),
                    _ => (),
                }
                self.lower_expr(&exprs[0])?;
                self.lower_expr(&exprs[1])?;
                self.block.insts.push(Inst(
                    match op {
                        Op::Minus => InstInfo::Sub,
                        Op::Percent => InstInfo::Mod,
                        Op::Plus => InstInfo::Add,
                        Op::Slash => InstInfo::Div,
                        Op::SlashSlash => InstInfo::Ediv,
                        Op::Star => InstInfo::Mul,
                        Op::EqEq => InstInfo::Eq,
                        Op::BangEq => InstInfo::Ne,
                        Op::Lt => InstInfo::Lt,
                        Op::Gt => InstInfo::Gt,
                        Op::LtEq => InstInfo::Lte,
                        Op::GtEq => InstInfo::Gte,
                        Op::Bar => InstInfo::BitOr,
                        Op::Amp => InstInfo::BitAnd,
                        Op::LtLt => InstInfo::Shl,
                        Op::GtGt => InstInfo::Shr,
                        Op::Tilde => InstInfo::BitNot,
                        Op::Caret => InstInfo::BitXor,
                        Op::Bang | Op::AmpAmp | Op::BarBar | Op::Dot | Op::DotHash => {
                            unreachable!()
                        }
                    },
                    *op_span,
                ));
            }
            Expr::Range { exprs, op_span } => {
                if let Some(start) = &exprs[0] {
                    self.lower_expr(start)?;
                } else {
                    self.lower_load_nil(*op_span);
                }
                if let Some(end) = &exprs[1] {
                    self.lower_expr(end)?;
                } else {
                    self.lower_load_nil(*op_span);
                }
                self.lower_load_nil(*op_span);
                let sig = sig::Pack::new(std::iter::repeat_n(sig::Arg::Value, 3));
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::RANGE, self.packtab.id(&sig)),
                    *op_span,
                ));
            }
            Expr::Lambda { func, do_span } => {
                self.lower_closure(func, do_span.unwrap_or_else(|| func.span()))?;
            }
            Expr::Call { arg0, args, .. } => match &**arg0 {
                Expr::Get {
                    object,
                    field,
                    dot_span,
                    ..
                } => {
                    let sym = match field {
                        GetVariant::Normal(span) => {
                            self.symtab.id(&self.bintab.id_str(self.file.str(*span)))
                        }
                        GetVariant::SpecialMethod { method, .. } => {
                            self.symtab.id(&self.bintab.id_str(method.sym()))
                        }
                        GetVariant::Private { res: Some(id), .. } => *id,
                        GetVariant::Private { res: None, .. } => unreachable!(),
                    };
                    self.lower_expr(object)?;
                    let mut sig = Vec::new();
                    for arg in args.iter() {
                        sig.push(self.lower_arg(arg)?);
                    }
                    let sig = sig::Pack::new(sig.into_iter());
                    self.block.insts.push(Inst(
                        InstInfo::MethodCall(sym, self.packtab.id(&sig)),
                        *dot_span,
                    ));
                }
                _ => {
                    self.lower_expr(arg0)?;
                    let mut sig = Vec::new();
                    for arg in args.iter() {
                        sig.push(self.lower_arg(arg)?);
                    }
                    let sig = sig::Pack::new(sig.into_iter());
                    self.block
                        .insts
                        .push(Inst(InstInfo::Call(self.packtab.id(&sig)), expr.span()));
                }
            },
            Expr::Get {
                object,
                field,
                dot_span,
                ..
            } => {
                self.lower_expr(object)?;
                let sym = match field {
                    GetVariant::Normal(span) => {
                        self.symtab.id(&self.bintab.id_str(self.file.str(*span)))
                    }
                    GetVariant::SpecialMethod { method, .. } => {
                        self.symtab.id(&self.bintab.id_str(method.sym()))
                    }
                    GetVariant::Private { res: Some(id), .. } => *id,
                    GetVariant::Private { res: None, .. } => unreachable!(),
                };
                self.block.insts.push(Inst(InstInfo::Get(sym), *dot_span))
            }
            Expr::Index { exprs, .. } => {
                self.lower_expr(&exprs[0])?;
                self.lower_expr(&exprs[1])?;
                self.block.insts.push(Inst(InstInfo::Index, expr.span()))
            }
            Expr::Array { elems, .. } => {
                let mut sig = Vec::new();
                for elem in elems.iter() {
                    sig.push(self.lower_array_elem(elem)?);
                }
                let sig = sig::Pack::new(sig.into_iter());
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::ARRAY, self.packtab.id(&sig)),
                    expr.span(),
                ));
            }
            Expr::Dict { elems, .. } => {
                let mut sig = Vec::new();
                for elem in elems.iter() {
                    let (arg, arg2) = self.lower_dict_elem(elem)?;
                    sig.push(arg);
                    if let Some(arg2) = arg2 {
                        sig.push(arg2)
                    }
                }
                let sig = sig::Pack::new(sig.into_iter());
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::DICT, self.packtab.id(&sig)),
                    expr.span(),
                ));
            }
        }
        Ok(())
    }

    fn lower_dict_elem(&mut self, elem: &'a DictElem) -> Result<(sig::Arg, Option<sig::Arg>)> {
        let int = OnceCell::new();
        let iter_sym = OnceCell::new();
        Ok(match elem {
            DictElem::Single(Single { expr, .. }) => {
                self.lower_expr(expr)?;
                (
                    sig::Arg::Key(*int.get_or_init(|| self.symtab.id(&self.bintab.id_str("int")))),
                    None,
                )
            }
            DictElem::Key(Key { key_span, expr, .. }) => {
                let cid = self.lower_const(&Const::Sym(*key_span));
                self.block
                    .insts
                    .push(Inst(InstInfo::LoadConst(cid), *key_span));
                self.lower_expr(expr)?;
                (sig::Arg::Value, Some(sig::Arg::Value))
            }
            DictElem::Pair(Pair { key, value, .. }) => {
                self.lower_expr(key)?;
                self.lower_expr(value)?;
                (sig::Arg::Value, Some(sig::Arg::Value))
            }
            DictElem::Expand(Expand { expr, .. }) => {
                self.lower_expr(expr)?;
                (
                    sig::Arg::Key(
                        *iter_sym.get_or_init(|| self.symtab.id(&self.bintab.id_str("iter"))),
                    ),
                    None,
                )
            }
            DictElem::If(node) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::ARGS, empty),
                    node.tbranch.span,
                ));

                let next = self.graph.alloc_block(self.block.func, self.block.scope);

                let start = self.bb;

                // Build control flow from inside out
                let mut fallback = next;

                // Handle final else branch if present
                if let Some((else_body, _)) = &node.else_branch {
                    let fscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    fallback = self.graph.alloc_block(self.block.func, fscope);
                    self.queue(Work {
                        bb: fallback,
                        ast: WorkAst::DictElems(else_body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });
                }

                // Process elif branches in reverse order
                for (elif_branch, _) in node.elif_branches.iter().rev() {
                    let current_fallback = fallback;
                    fallback = self.graph.alloc_block(self.block.func, self.block.scope);

                    let tscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    let tid = self.graph.alloc_block(self.block.func, tscope);
                    self.queue(Work {
                        bb: tid,
                        ast: WorkAst::DictElems(&elif_branch.body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });

                    self.switch(fallback);
                    self.lower_expr(&elif_branch.cond)?;
                    self.block.term = Term(TermInfo::If(tid, current_fallback), elif_branch.span);
                    self.link(tid);
                    self.link(current_fallback);
                }

                self.switch(start);

                // Finally, process initial if branch
                let current_fallback = fallback;
                self.lower_expr(&node.tbranch.cond)?;
                let tscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &[],
                    self.origintab,
                );
                let tid = self.graph.alloc_block(self.block.func, tscope);
                self.queue(Work {
                    bb: tid,
                    ast: WorkAst::DictElems(&node.tbranch.body),
                    params: Params {
                        bind: None,
                        bind_params: None,
                        unpack: None,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(next),
                        break_id: self.params.break_id,
                        break_result: self.params.break_result,
                        continue_id: self.params.continue_id,
                        exit_id: self.params.exit_id,
                    },
                });

                self.block.term = Term(TermInfo::If(tid, current_fallback), node.tbranch.span);
                self.link(tid);
                self.link(current_fallback);
                self.switch(next);

                (sig::Arg::Pack, None)
            }
            DictElem::For(For {
                bind,
                expr,
                body,
                iter,
                for_span,
                ..
            }) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ARGS, empty), *for_span));

                let sig = if let Some(expr) = expr {
                    self.lower_expr(expr)?;
                    let call = sig::Pack::new([sig::Arg::Value].into_iter());
                    self.packtab.id(&call)
                } else {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ITER, sig), *for_span));
                self.lower_store_res(
                    iter.as_ref().expect("unresolved for iterator"),
                    *for_span,
                    false,
                );
                let advance = self.graph.alloc_block(self.block.func, self.block.scope);
                let next = self.graph.alloc_block(self.block.func, self.block.scope);
                let bscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &body.vars,
                    self.origintab,
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let (binds, unpack, bind_params) = match bind {
                    Pattern::Ident(_) => (
                        vec![self.resolve_var_in_scope(self.graph.scope(bscope), 0, 0)],
                        None,
                        None,
                    ),
                    Pattern::Unpack(params) => {
                        let unpack = self.lower_params(params)?;
                        let sig = self.unpacktab.id(&unpack);
                        (
                            self.unpack_order_in_scope(self.graph.scope(bscope), params, sig),
                            Some(sig),
                            Some(params.as_slice()),
                        )
                    }
                };
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::DictElems(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
                        unpack,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(advance),
                        break_id: None,
                        break_result: false,
                        continue_id: None,
                        exit_id: self.params.exit_id,
                    },
                });
                self.block.term = Term(TermInfo::Branch(advance), *for_span);
                self.link(advance);
                self.switch(advance);
                self.lower_load(iter.as_ref().unwrap(), *for_span);
                self.block.insts.push(Inst(InstInfo::Next, *for_span));
                self.block.term = Term(TermInfo::If(bodyid, next), *for_span);
                self.link(bodyid);
                self.link(next);
                self.switch(next);
                self.block.insts.push(Inst(InstInfo::Pop, *for_span));
                (sig::Arg::Pack, None)
            }
        })
    }

    fn lower_array_elem(&mut self, elem: &'a ArrayElem) -> Result<sig::Arg> {
        let iter_sym = OnceCell::new();
        Ok(match elem {
            ArrayElem::Single(Single { expr, .. }) => {
                self.lower_expr(expr)?;
                sig::Arg::Value
            }
            ArrayElem::Expand(Expand { expr, .. }) => {
                self.lower_expr(expr)?;
                sig::Arg::Key(*iter_sym.get_or_init(|| self.symtab.id(&self.bintab.id_str("iter"))))
            }
            ArrayElem::If(node) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::ARGS, empty),
                    node.tbranch.span,
                ));

                let next = self.graph.alloc_block(self.block.func, self.block.scope);

                let start = self.bb;

                // Build control flow from inside out
                let mut fallback = next;

                // Handle final else branch if present
                if let Some((else_body, _)) = &node.else_branch {
                    let fscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    fallback = self.graph.alloc_block(self.block.func, fscope);
                    self.queue(Work {
                        bb: fallback,
                        ast: WorkAst::ArrayElems(else_body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });
                }

                // Process elif branches in reverse order
                for (elif_branch, _) in node.elif_branches.iter().rev() {
                    let current_fallback = fallback;
                    fallback = self.graph.alloc_block(self.block.func, self.block.scope);

                    let tscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    let tid = self.graph.alloc_block(self.block.func, tscope);
                    self.queue(Work {
                        bb: tid,
                        ast: WorkAst::ArrayElems(&elif_branch.body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });

                    self.switch(fallback);
                    self.lower_expr(&elif_branch.cond)?;
                    self.block.term = Term(TermInfo::If(tid, current_fallback), elif_branch.span);
                    self.link(tid);
                    self.link(current_fallback);
                }

                self.switch(start);

                let current_fallback = fallback;
                self.lower_expr(&node.tbranch.cond)?;
                let tscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &[],
                    self.origintab,
                );
                let tid = self.graph.alloc_block(self.block.func, tscope);
                self.queue(Work {
                    bb: tid,
                    ast: WorkAst::ArrayElems(&node.tbranch.body),
                    params: Params {
                        bind: None,
                        bind_params: None,
                        unpack: None,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(next),
                        break_id: self.params.break_id,
                        break_result: self.params.break_result,
                        continue_id: self.params.continue_id,
                        exit_id: self.params.exit_id,
                    },
                });

                self.block.term = Term(TermInfo::If(tid, current_fallback), node.tbranch.span);
                self.link(tid);
                self.link(current_fallback);
                self.switch(next);

                sig::Arg::Pack
            }
            ArrayElem::For(For {
                bind,
                expr,
                body,
                iter,
                for_span,
                ..
            }) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ARGS, empty), *for_span));

                let sig = if let Some(expr) = expr {
                    self.lower_expr(expr)?;
                    let call = sig::Pack::new([sig::Arg::Value].into_iter());
                    self.packtab.id(&call)
                } else {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ITER, sig), *for_span));
                self.lower_store_res(
                    iter.as_ref().expect("unresolved for iterator"),
                    *for_span,
                    false,
                );
                let advance = self.graph.alloc_block(self.block.func, self.block.scope);
                let next = self.graph.alloc_block(self.block.func, self.block.scope);
                let bscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &body.vars,
                    self.origintab,
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let (binds, unpack, bind_params) = match bind {
                    Pattern::Ident(_) => (
                        vec![self.resolve_var_in_scope(self.graph.scope(bscope), 0, 0)],
                        None,
                        None,
                    ),
                    Pattern::Unpack(params) => {
                        let unpack = self.lower_params(params)?;
                        let sig = self.unpacktab.id(&unpack);
                        (
                            self.unpack_order_in_scope(self.graph.scope(bscope), params, sig),
                            Some(sig),
                            Some(params.as_slice()),
                        )
                    }
                };
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::ArrayElems(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
                        unpack,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(advance),
                        break_id: None,
                        break_result: false,
                        continue_id: None,
                        exit_id: self.params.exit_id,
                    },
                });
                self.block.term = Term(TermInfo::Branch(advance), *for_span);
                self.link(advance);
                self.switch(advance);
                self.lower_load(iter.as_ref().unwrap(), *for_span);
                self.block.insts.push(Inst(InstInfo::Next, *for_span));
                self.block.term = Term(TermInfo::If(bodyid, next), *for_span);
                self.link(bodyid);
                self.link(next);
                self.switch(next);
                self.block.insts.push(Inst(InstInfo::Pop, *for_span));
                sig::Arg::Pack
            }
        })
    }

    fn lower_arg(&mut self, arg: &'a Arg) -> Result<sig::Arg> {
        Ok(match arg {
            Arg::Pos(Single { expr, .. }) => {
                self.lower_expr(expr)?;
                sig::Arg::Value
            }
            Arg::Key(Key { key_span, expr, .. }) => {
                self.lower_expr(expr)?;
                sig::Arg::Key(
                    self.symtab
                        .id(&self.bintab.id_str(self.file.str(*key_span))),
                )
            }
            Arg::Expand(Expand { expr, .. }) => {
                self.lower_expr(expr)?;
                sig::Arg::Pack
            }
            Arg::For(For {
                bind,
                expr,
                body,
                iter,
                for_span,
                ..
            }) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ARGS, empty), *for_span));

                let sig = if let Some(expr) = expr {
                    self.lower_expr(expr)?;
                    let call = sig::Pack::new([sig::Arg::Value].into_iter());
                    self.packtab.id(&call)
                } else {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::ITER, sig), *for_span));
                self.lower_store_res(
                    iter.as_ref().expect("unresolved for iterator"),
                    *for_span,
                    false,
                );
                let advance = self.graph.alloc_block(self.block.func, self.block.scope);
                let next = self.graph.alloc_block(self.block.func, self.block.scope);
                let bscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &body.vars,
                    self.origintab,
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let (binds, unpack, bind_params) = match bind {
                    Pattern::Ident(_) => (
                        vec![self.resolve_var_in_scope(self.graph.scope(bscope), 0, 0)],
                        None,
                        None,
                    ),
                    Pattern::Unpack(params) => {
                        let unpack = self.lower_params(params)?;
                        let sig = self.unpacktab.id(&unpack);
                        (
                            self.unpack_order_in_scope(self.graph.scope(bscope), params, sig),
                            Some(sig),
                            Some(params.as_slice()),
                        )
                    }
                };
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::Args(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
                        unpack,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(advance),
                        break_id: None,
                        break_result: false,
                        continue_id: None,
                        exit_id: self.params.exit_id,
                    },
                });
                self.block.term = Term(TermInfo::Branch(advance), *for_span);
                self.link(advance);
                self.switch(advance);
                self.lower_load(iter.as_ref().unwrap(), *for_span);
                self.block.insts.push(Inst(InstInfo::Next, *for_span));
                self.block.term = Term(TermInfo::If(bodyid, next), *for_span);
                self.link(bodyid);
                self.link(next);
                self.switch(next);
                self.block.insts.push(Inst(InstInfo::Pop, *for_span));
                sig::Arg::Pack
            }
            Arg::If(node) => {
                let empty = {
                    let call = sig::Pack::new([].into_iter());
                    self.packtab.id(&call)
                };
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::ARGS, empty),
                    node.tbranch.span,
                ));

                let next = self.graph.alloc_block(self.block.func, self.block.scope);

                // Build control flow from inside out
                let mut fallback = next;

                // Handle final else branch if present
                if let Some((else_body, _)) = &node.else_branch {
                    let fscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    fallback = self.graph.alloc_block(self.block.func, fscope);
                    self.queue(Work {
                        bb: fallback,
                        ast: WorkAst::Args(else_body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });
                }

                // Process elif branches in reverse order
                for (elif_branch, _) in node.elif_branches.iter().rev() {
                    let current_fallback = fallback;
                    fallback = self.graph.alloc_block(self.block.func, self.block.scope);

                    // Set up if condition and true branch
                    self.lower_expr(&elif_branch.cond)?;
                    let tscope = self.graph.alloc_scope(
                        false,
                        false,
                        self.block.func,
                        Some(self.block.scope),
                        &[],
                        self.origintab,
                    );
                    let tid = self.graph.alloc_block(self.block.func, tscope);
                    self.queue(Work {
                        bb: tid,
                        ast: WorkAst::Args(&elif_branch.body),
                        params: Params {
                            bind: None,
                            bind_params: None,
                            unpack: None,
                            mode: self.params.mode.clone(),
                            is_top_level: false,
                            next_id: Some(next),
                            break_id: self.params.break_id,
                            break_result: self.params.break_result,
                            continue_id: self.params.continue_id,
                            exit_id: self.params.exit_id,
                        },
                    });

                    // Create conditional jump
                    self.block.term = Term(TermInfo::If(tid, current_fallback), elif_branch.span);
                    self.link(tid);
                    self.link(current_fallback);
                    self.switch(fallback);
                }

                // Finally, process initial if branch
                let current_fallback = fallback;
                self.lower_expr(&node.tbranch.cond)?;
                let tscope = self.graph.alloc_scope(
                    false,
                    false,
                    self.block.func,
                    Some(self.block.scope),
                    &[],
                    self.origintab,
                );
                let tid = self.graph.alloc_block(self.block.func, tscope);
                self.queue(Work {
                    bb: tid,
                    ast: WorkAst::Args(&node.tbranch.body),
                    params: Params {
                        bind: None,
                        bind_params: None,
                        unpack: None,
                        mode: self.params.mode.clone(),
                        is_top_level: false,
                        next_id: Some(next),
                        break_id: self.params.break_id,
                        break_result: self.params.break_result,
                        continue_id: self.params.continue_id,
                        exit_id: self.params.exit_id,
                    },
                });

                self.block.term = Term(TermInfo::If(tid, current_fallback), node.tbranch.span);
                self.link(tid);
                self.link(current_fallback);
                self.switch(next);

                sig::Arg::Pack
            }
            Arg::DynamicKey { .. } => unreachable!(),
        })
    }

    fn lower_let(&mut self, node: &'a Let, want_result: bool) -> Result<()> {
        let Let { bind, rhs, .. } = node;

        self.lower_prim_stmt(rhs, true)?;
        self.lower_pattern(bind, want_result)?;
        Ok(())
    }

    fn lower_bind(&mut self, node: &'a Bind, want_result: bool) -> Result<()> {
        let Bind { bind, expr, .. } = node;

        self.lower_expr(expr)?;
        self.lower_pattern(bind, want_result)?;
        Ok(())
    }

    fn lower_pattern(&mut self, bind: &'a Pattern, want_result: bool) -> Result<()> {
        match bind {
            Pattern::Ident(Ident { res, span }) => {
                let res = res.as_ref().expect("unresolved assignment lhs");
                self.lower_store_res(res, *span, want_result);
            }
            Pattern::Unpack(params) => {
                let unpack = self.lower_params(params)?;
                let sig = self.unpacktab.id(&unpack);
                let span = bind.span();
                if want_result {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                }
                self.block.insts.push(Inst(InstInfo::Unpack(sig), span));
                for var in self.unpack_order(params, sig).into_iter() {
                    self.lower_store(span, var);
                }
                self.lower_non_const_defaults(params, span)?;
            }
        }
        Ok(())
    }

    fn lower_assign(&mut self, node: &'a Assign, want_result: bool) -> Result<()> {
        let Assign {
            lhs,
            rhs,
            equal_span,
        } = node;
        let span = node.span();

        match lhs {
            LValue::Ident(id) => {
                self.lower_prim_stmt(rhs, true)?;
                let res = id.res.as_ref().expect("unresolved assignment lhs");
                let var = self.resolve_var(res.index, res.depth);
                if want_result {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                }
                match var {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreLocal(index), *equal_span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreUpvar(index, depth), *equal_span)),
                }
                Ok(())
            }
            LValue::Field { object, field, .. } => {
                self.lower_expr(object)?;
                self.lower_prim_stmt(rhs, true)?;
                if want_result {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                    self.block.insts.push(Inst(InstInfo::Swap(1, 2), span));
                }
                self.block.insts.push(Inst(
                    InstInfo::Set(self.symtab.id(&self.bintab.id_str(self.file.str(*field)))),
                    *equal_span,
                ));
                Ok(())
            }
            LValue::PrivateField {
                object,
                res: Some(id),
                ..
            } => {
                self.lower_expr(object)?;
                self.lower_prim_stmt(rhs, true)?;
                if want_result {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                    self.block.insts.push(Inst(InstInfo::Swap(1, 2), span));
                }
                self.block.insts.push(Inst(InstInfo::Set(*id), *equal_span));
                Ok(())
            }
            LValue::PrivateField { res: None, .. } => unreachable!(),
            LValue::Index { exprs, .. } => {
                self.lower_expr(&exprs[0])?;
                self.lower_expr(&exprs[1])?;
                self.lower_prim_stmt(rhs, true)?;
                if want_result {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                    self.block.insts.push(Inst(InstInfo::Swap(1, 2), span));
                    self.block.insts.push(Inst(InstInfo::Swap(2, 3), span));
                }
                self.block.insts.push(Inst(InstInfo::Assign, *equal_span));
                Ok(())
            }
        }
    }

    fn lower_if(&mut self, node: &'a If<Block>, want_result: bool) -> Result<()> {
        let complete = node.else_branch.is_some();
        let next = self.graph.alloc_block(self.block.func, self.block.scope);

        let start = self.bb;

        // Build the control flow structure from the inside out
        let mut fallback = next;

        // Handle final else branch if present
        if let Some((else_block, _)) = &node.else_branch {
            let fscope = self.graph.alloc_scope(
                false,
                false,
                self.block.func,
                Some(self.block.scope),
                &else_block.vars,
                self.origintab,
            );
            fallback = self.graph.alloc_block(self.block.func, fscope);
            self.queue(Work {
                bb: fallback,
                ast: WorkAst::Block(else_block, want_result && complete),
                params: Params {
                    bind: None,
                    bind_params: None,
                    unpack: None,
                    mode: self.params.mode.clone(),
                    is_top_level: false,
                    next_id: Some(next),
                    break_id: self.params.break_id,
                    break_result: self.params.break_result,
                    continue_id: self.params.continue_id,
                    exit_id: self.params.exit_id,
                },
            });
        }

        // Process elif branches in reverse order (last to first)
        for (elif_branch, _) in node.elif_branches.iter().rev() {
            let current_fallback = fallback;
            fallback = self.graph.alloc_block(self.block.func, self.block.scope);

            let tscope = self.graph.alloc_scope(
                false,
                false,
                self.block.func,
                Some(self.block.scope),
                &elif_branch.body.vars,
                self.origintab,
            );
            let tid = self.graph.alloc_block(self.block.func, tscope);
            self.queue(Work {
                bb: tid,
                ast: WorkAst::Block(&elif_branch.body, want_result && complete),
                params: Params {
                    bind: None,
                    bind_params: None,
                    unpack: None,
                    mode: self.params.mode.clone(),
                    is_top_level: false,
                    next_id: Some(next),
                    break_id: self.params.break_id,
                    break_result: self.params.break_result,
                    continue_id: self.params.continue_id,
                    exit_id: self.params.exit_id,
                },
            });

            self.switch(fallback);
            self.lower_expr(&elif_branch.cond)?;
            self.block.term = Term(TermInfo::If(tid, current_fallback), elif_branch.span);
            self.link(tid);
            self.link(current_fallback);
        }

        // Finally, process the initial if branch
        self.switch(start);
        let current_fallback = fallback;
        self.lower_expr(&node.tbranch.cond)?;
        let tscope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.tbranch.body.vars,
            self.origintab,
        );
        let tid = self.graph.alloc_block(self.block.func, tscope);
        self.queue(Work {
            bb: tid,
            ast: WorkAst::Block(&node.tbranch.body, want_result && complete),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(next),
                break_id: self.params.break_id,
                break_result: self.params.break_result,
                continue_id: self.params.continue_id,
                exit_id: self.params.exit_id,
            },
        });

        self.block.term = Term(TermInfo::If(tid, current_fallback), node.tbranch.span);
        self.link(tid);
        self.link(current_fallback);
        self.switch(next);

        if want_result && !complete {
            self.lower_load_nil(node.span())
        }
        Ok(())
    }

    fn lower_closure(&mut self, func: &'a Function, span: Span) -> Result<()> {
        let unpack = self.lower_params(&func.params)?;
        let sig = self.unpacktab.id(&unpack);
        let fid = self.graph.alloc_func(
            sig,
            None,
            &func.body.vars,
            Some(self.block.scope),
            self.origintab,
        );
        let (enter, exit) = {
            let f = self.graph.func(fid);
            (f.enter, f.exit)
        };
        self.queue(Work {
            bb: enter,
            ast: WorkAst::Function(func, sig),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: None,
                break_id: None,
                break_result: false,
                continue_id: None,
                exit_id: exit,
            },
        });
        self.block.insts.push(Inst(InstInfo::Close(fid), span));
        Ok(())
    }

    fn lower_try(&mut self, node: &'a Try, want_result: bool) -> Result<()> {
        let span = node.try_span;
        let mut sig_args = Vec::new();

        // Body closure (0 params)
        self.lower_closure(&node.body, span)?;
        sig_args.push(sig::Arg::Value);

        // Catch-all closure or nil (fixed arg, before typed pairs)
        let catch_all_handler = node.handlers.iter().find(|h| h.class_expr.is_none());
        if let Some(handler) = catch_all_handler {
            self.lower_closure(&handler.func, handler.catch_span)?;
        } else {
            self.lower_load_nil(span);
        }
        sig_args.push(sig::Arg::Value);

        // Finally closure or nil (fixed arg, before typed pairs)
        if let Some((finally_func, finally_span)) = &node.finally {
            self.lower_closure(finally_func, *finally_span)?;
        } else {
            self.lower_load_nil(span);
        }
        sig_args.push(sig::Arg::Value);

        // Typed catch handlers: class_expr, handler_closure pairs (trailing, iterated live)
        for handler in &node.handlers {
            if let Some(class_expr) = &handler.class_expr {
                self.lower_expr(class_expr)?;
                sig_args.push(sig::Arg::Value);
                self.lower_closure(&handler.func, handler.catch_span)?;
                sig_args.push(sig::Arg::Value);
            }
        }

        // Emit Guard builtin call
        let sig = sig::Pack::new(sig_args.into_iter());
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::GUARD, self.packtab.id(&sig)),
            span,
        ));

        if !want_result {
            self.block.insts.push(Inst(InstInfo::Pop, span));
        }
        Ok(())
    }

    fn lower_while(&mut self, node: &'a While, want_result: bool) -> Result<()> {
        let test = self.graph.alloc_block(self.block.func, self.block.scope);
        let next = self.graph.alloc_block(self.block.func, self.block.scope);
        let bscope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.body.vars,
            self.origintab,
        );
        let bodyid = self.graph.alloc_block(self.block.func, bscope);
        let span = node.while_span;
        self.queue(Work {
            bb: bodyid,
            ast: WorkAst::Block(&node.body, false),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(test),
                break_id: Some(next),
                break_result: false,
                continue_id: Some(test),
                exit_id: self.params.exit_id,
            },
        });
        self.block.term = Term(TermInfo::Branch(test), span);
        self.link(test);
        self.switch(test);
        self.lower_expr(&node.cond)?;
        self.block.term = Term(TermInfo::If(bodyid, next), span);
        self.link(bodyid);
        self.link(next);
        self.switch(next);
        if want_result {
            self.lower_load_nil(span)
        }
        Ok(())
    }

    fn lower_for(&mut self, node: &'a For<Block>, want_result: bool) -> Result<()> {
        let span = node.span();
        let sig = if let Some(expr) = &node.expr {
            self.lower_expr(expr)?;
            let call = sig::Pack::new([sig::Arg::Value].into_iter());
            self.packtab.id(&call)
        } else {
            let call = sig::Pack::new([].into_iter());
            self.packtab.id(&call)
        };
        self.block
            .insts
            .push(Inst(InstInfo::Builtin(builtin::ITER, sig), span));
        self.lower_store_res(
            node.iter.as_ref().expect("unresolved for iterator"),
            span,
            false,
        );
        let advance = self.graph.alloc_block(self.block.func, self.block.scope);
        let next = self.graph.alloc_block(self.block.func, self.block.scope);
        let bscope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.body.vars,
            self.origintab,
        );
        let bodyid = self.graph.alloc_block(self.block.func, bscope);
        let (binds, unpack, bind_params) = match &node.bind {
            Pattern::Ident(_) => (
                vec![self.resolve_var_in_scope(self.graph.scope(bscope), 0, 0)],
                None,
                None,
            ),
            Pattern::Unpack(params) => {
                let unpack = self.lower_params(params)?;
                let sig = self.unpacktab.id(&unpack);
                (
                    self.unpack_order_in_scope(self.graph.scope(bscope), params, sig),
                    Some(sig),
                    Some(params.as_slice()),
                )
            }
        };
        // FIXME: include span of keyword, not of block
        self.queue(Work {
            bb: bodyid,
            ast: WorkAst::Block(&node.body, false),
            params: Params {
                bind: Some(binds),
                bind_params,
                unpack,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(advance),
                break_id: Some(next),
                break_result: true,
                continue_id: Some(advance),
                exit_id: self.params.exit_id,
            },
        });
        self.block.term = Term(TermInfo::Branch(advance), span);
        self.link(advance);
        self.switch(advance);
        self.lower_load(node.iter.as_ref().unwrap(), span);
        self.block.insts.push(Inst(InstInfo::Next, span));
        self.block.term = Term(TermInfo::If(bodyid, next), span);
        self.link(bodyid);
        self.link(next);
        self.switch(next);
        if !want_result {
            self.block.insts.push(Inst(InstInfo::Pop, span));
        }
        Ok(())
    }

    fn lower_params(&mut self, params: &'a [Param]) -> Result<sig::Unpack> {
        let mut required = 0;
        let mut optional = Vec::new();
        let mut keys = Vec::new();
        let mut variadic = dolang_bytecode::Variadic::None;

        for param in params.iter() {
            match param {
                Param::Pos { default: None, .. } => required += 1,
                Param::Pos {
                    default: Some(default),
                    ..
                } => optional.push(self.lower_default_const(default)),
                Param::Key {
                    key_span, default, ..
                } => {
                    let constid = default
                        .as_ref()
                        .map(|default| self.lower_default_const(default));
                    keys.push(sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Sym(
                            self.symtab
                                .id(&self.bintab.id_str(self.file.str(*key_span))),
                        ),
                        default: constid,
                    })
                }
                Param::ConstKey {
                    key_const, default, ..
                } => {
                    let constid_default = default
                        .as_ref()
                        .map(|default| self.lower_default_const(default));

                    // Lower the key constant value
                    let key_const_id = self.lower_const(key_const);

                    keys.push(sig::UnpackKey {
                        kind: sig::UnpackKeyKind::Const(key_const_id),
                        default: constid_default,
                    })
                }
                Param::Rest { ident, .. } => {
                    assert_eq!(variadic, dolang_bytecode::Variadic::None);
                    variadic = if ident.is_some() {
                        dolang_bytecode::Variadic::Capture
                    } else {
                        dolang_bytecode::Variadic::Discard
                    };
                }
            }
        }

        Ok(sig::Unpack::new(required, optional, keys, variadic))
    }

    fn lower_non_const_defaults(&mut self, params: &'a [Param], span: Span) -> Result<()> {
        for param in params {
            let (default, ident) = match param {
                Param::Pos {
                    default: Some(d),
                    ident,
                    ..
                }
                | Param::Key {
                    default: Some(d),
                    ident,
                    ..
                }
                | Param::ConstKey {
                    default: Some(d),
                    ident,
                    ..
                } if d.fold.is_none() => (d, ident),
                _ => continue,
            };

            let res = ident.res.as_ref().expect("unresolved param");
            let var = self.resolve_var(res.index, res.depth);

            // Load the current value of this param
            self.lower_load(res, span);
            // Load sentinel and compare
            let sentinel = self.sentinel_const();
            self.block
                .insts
                .push(Inst(InstInfo::LoadConst(sentinel), span));
            self.block.insts.push(Inst(InstInfo::Eq, span));

            // Branch: if true (sentinel), evaluate default; else skip
            let eval_bb = self.graph.alloc_block(self.block.func, self.block.scope);
            let skip_bb = self.graph.alloc_block(self.block.func, self.block.scope);
            self.block.term = Term(TermInfo::If(eval_bb, skip_bb), span);
            self.link(eval_bb);
            self.link(skip_bb);

            // Evaluate default expression
            self.switch(eval_bb);
            self.lower_expr(&default.expr)?;
            self.lower_store(span, var);
            self.block.term = Term(TermInfo::Branch(skip_bb), span);
            self.link(skip_bb);

            // Continue in skip block
            self.switch(skip_bb);
        }
        Ok(())
    }

    fn lower_default_const(&mut self, default: &ParamDefault) -> constant::Id {
        match &default.fold {
            Some(fold) => self.lower_const(fold),
            None => self.sentinel_const(),
        }
    }

    fn sentinel_const(&mut self) -> constant::Id {
        if let Some(id) = *self.sentinel_const {
            id
        } else {
            let sym_id = self.symtab.fresh(self.bintab.id_str("default"));
            let id = self.consttab.sym(sym_id);
            *self.sentinel_const = Some(id);
            id
        }
    }

    fn lower_const(&mut self, node: &Const) -> constant::Id {
        match node {
            Const::Str(str) => self.consttab.str(self.bintab.id_str(str)),
            Const::Bin(bytes) => self.consttab.bin(self.bintab.id(bytes)),
            Const::Int(v) => self.consttab.int(*v),
            Const::F64(v) => self.consttab.f64(*v),
            Const::Bool(v) => self.consttab.bool(*v),
            Const::Nil => self.consttab.nil(),
            Const::Sym(span) => self
                .consttab
                .sym(self.symtab.id(&self.bintab.id_str(self.file.str(*span)))),
            Const::Error => unreachable!(),
        }
    }

    fn lower_def(&mut self, node: &'a Def, want_result: bool) -> Result<()> {
        self.lower_decorator_exprs(&node.decorators)?;
        let unpack = self.lower_params(&node.func.params)?;
        let (name, res) = match &node.variant {
            DefVariant::Normal(ident) => (ident.span, &ident.res),
            DefVariant::Special(_, span, res) => {
                (span.before_left_char() | span.after_right_char(), res)
            }
        };
        let sig = self.unpacktab.id(&unpack);
        let fid = self.graph.alloc_func(
            sig,
            Some(name),
            &node.func.body.vars,
            Some(self.block.scope),
            self.origintab,
        );
        let (enter, exit) = {
            let func = self.graph.func(fid);
            (func.enter, func.exit)
        };
        self.queue(Work {
            bb: enter,
            ast: WorkAst::Function(&node.func, sig),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: None,
                break_id: None,
                break_result: false,
                continue_id: None,
                exit_id: exit,
            },
        });
        self.block
            .insts
            .push(Inst(InstInfo::Close(fid), node.def_span));
        self.apply_decorators(&node.decorators);
        let res = res.as_ref().expect("unresolved assignment lhs");
        let var = self.resolve_var(res.index, res.depth);
        if want_result {
            self.block.insts.push(Inst(InstInfo::Dup, node.def_span));
        }
        match var {
            Var::Local(index) => self
                .block
                .insts
                .push(Inst(InstInfo::StoreLocal(index), node.def_span)),
            Var::Upvar(index, depth) => self
                .block
                .insts
                .push(Inst(InstInfo::StoreUpvar(index, depth), node.def_span)),
        }
        Ok(())
    }

    fn lower_decorator_exprs(&mut self, decorators: &'a [Decorator]) -> Result<()> {
        for decorator in decorators {
            self.lower_expr(&decorator.expr)?;
        }
        Ok(())
    }

    fn apply_decorators(&mut self, decorators: &'a [Decorator]) {
        if decorators.is_empty() {
            return;
        }
        let sig = sig::Pack::new([sig::Arg::Value].into_iter());
        let sig = self.packtab.id(&sig);
        for decorator in decorators.iter().rev() {
            self.block
                .insts
                .push(Inst(InstInfo::Call(sig), decorator.open_span));
        }
    }

    fn lower_class(&mut self, node: &'a Class, want_result: bool) -> Result<()> {
        let span = node.class_span;

        self.lower_decorator_exprs(&node.decorators)?;

        // Push class name as a string constant (first arg to CLASS_CREATE)
        let name = self.file.str(node.ident.span).to_owned();
        let cid = self.consttab.str(self.bintab.id_str(&name));
        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));

        // Evaluate superclass expressions (if any)
        for super_expr in &node.super_exprs {
            self.lower_expr(super_expr)?;
        }

        // Allocate a nested scope for the class body (not a function scope)
        let class_scope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.body.vars,
            self.origintab,
        );
        self.graph.scope_mut(class_scope).class_name = Some(node.ident.span);
        let class_block = self.graph.alloc_block(self.block.func, class_scope);
        let next = self.graph.alloc_block(self.block.func, self.block.scope);

        // Queue the class body for lowering (will emit Reify at the end)
        self.queue(Work {
            bb: class_block,
            ast: WorkAst::ClassBody(&node.body),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(next),
                break_id: self.params.break_id,
                break_result: self.params.break_result,
                continue_id: self.params.continue_id,
                exit_id: self.params.exit_id,
            },
        });

        // Branch into the class body
        self.block.term = Term(TermInfo::Branch(class_block), span);
        self.link(class_block);

        // Continue after the class body — reified module is on the stack
        self.switch(next);

        // Call CLASS_CREATE builtin
        // Stack: name_str [super0? super1? ...] reified_module
        let class_sig = sig::Pack::new(std::iter::repeat_n(
            sig::Arg::Value,
            node.super_exprs.len() + 2,
        ));
        let class_sig = self.packtab.id(&class_sig);
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::CLASS_CREATE, class_sig),
            span,
        ));
        self.apply_decorators(&node.decorators);

        // Store class object into the class name variable
        let res = node.ident.res.as_ref().expect("unresolved class name");
        self.lower_store_res(res, node.ident.span, want_result);

        Ok(())
    }

    /// Lower the body of a class definition. Executes all statements, then
    /// reifies the scope into a module object left on the stack.
    fn lower_class_body(&mut self, block: &'a Block) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        let span = block.span();

        // Prologue: always push an upvar frame so Reify can always produce a module
        // (carrying the program reference needed for ClassObject._program).
        self.block
            .insts
            .push(Inst(InstInfo::PushUpvars(scope.caps), span));

        // Lower each statement (none produce a result)
        for stmt in block.stmts.iter() {
            if self.lower_stmt(stmt, false)? {
                return Ok(());
            }
        }

        // Epilogue: reify the scope into a module object (empty sig when no captures)
        let module_sig = sig::Pack::new(block.vars.iter().filter(|v| v.captured).map(|v| {
            if v.exported {
                sig::Arg::Key(v.sym)
            } else {
                sig::Arg::Value
            }
        }));
        let sig = self.packtab.id(&module_sig);
        self.block.insts.push(Inst(InstInfo::Reify(sig), span));

        // Branch to next block
        let next = self.params.next_id.expect("class body must have next_id");
        self.block.term = Term(TermInfo::Branch(next), span);
        self.link(next);

        Ok(())
    }

    fn lower_load_nil(&mut self, span: Span) {
        self.block
            .insts
            .push(Inst(InstInfo::LoadConst(self.consttab.nil()), span));
    }

    fn lower_import(&mut self, import: &Import, want_result: bool) -> Result<()> {
        let span = import.span();
        for import in import.0.iter() {
            let module = match import {
                ImportElement::ModuleAsIs { module, .. }
                | ImportElement::ModuleRenamed { module, .. } => *module,
                ImportElement::Items { module, .. } => *module,
            };

            match import {
                ImportElement::ModuleAsIs { bind, insert, .. } => {
                    if *insert {
                        let cid = self.consttab.str(self.bintab.id_str(self.file.str(module)));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        self.lower_load(bind.res.as_ref().unwrap(), span);
                        let insert = self.symtab.id(&self.bintab.id_str("insert"));
                        let call =
                            sig::Pack::new([sig::Arg::Key(insert), sig::Arg::Value].into_iter());
                        let sig = self.packtab.id(&call);
                        self.block
                            .insts
                            .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));
                        self.block.insts.push(Inst(InstInfo::Pop, span));
                    } else {
                        let cid = self.consttab.str(self.bintab.id_str(self.file.str(module)));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        let module = self.symtab.id(&self.bintab.id_str("module"));
                        let call = sig::Pack::new([sig::Arg::Key(module)].into_iter());
                        let sig = self.packtab.id(&call);
                        self.block
                            .insts
                            .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));
                        self.lower_store_res(
                            bind.res.as_ref().expect("unresolved import module"),
                            bind.span,
                            false,
                        );
                    }
                }
                ImportElement::ModuleRenamed { bind, .. } => {
                    let cid = self.consttab.str(self.bintab.id_str(self.file.str(module)));
                    self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                    let get = self.symtab.id(&self.bintab.id_str("get"));
                    let call = sig::Pack::new([sig::Arg::Key(get)].into_iter());
                    let sig = self.packtab.id(&call);
                    self.block
                        .insts
                        .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));
                    self.lower_store_res(
                        bind.res.as_ref().expect("unresolved import module"),
                        bind.span,
                        false,
                    );
                }
                ImportElement::Items { items, .. } => {
                    let cid = self.consttab.str(self.bintab.id_str(self.file.str(module)));
                    self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                    let get = self.symtab.id(&self.bintab.id_str("get"));
                    let call = sig::Pack::new([sig::Arg::Key(get)].into_iter());
                    let sig = self.packtab.id(&call);
                    self.block
                        .insts
                        .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));

                    for (i, item) in items.iter().enumerate() {
                        let (item_span, bind) = match item {
                            ImportItem::Renamed { item, bind, .. } => (*item, bind),
                            ImportItem::AsIs { bind, .. } => (bind.span, bind),
                        };
                        if i + 1 != items.len() {
                            self.block.insts.push(Inst(InstInfo::Dup, span));
                        }
                        let sym = self
                            .symtab
                            .id(&self.bintab.id_str(self.file.str(item_span)));
                        self.block.insts.push(Inst(InstInfo::Get(sym), span));
                        self.lower_store_res(
                            bind.res.as_ref().expect("unresolved import item"),
                            bind.span,
                            false,
                        );
                    }
                }
            }
        }

        if want_result {
            let cid = self.consttab.nil();
            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
        }

        Ok(())
    }

    fn lower_scope_leave(&mut self, target: cfg::BlockId, span: Span) -> Result<()> {
        let cur = self.graph.scope(self.block.scope);
        if target == self.params.exit_id {
            for _ in 0..cur.func_upvar_depth {
                self.block.insts.push(Inst(InstInfo::PopUpvars, span));
            }
            if self.params.next_id.is_none() {
                self.block.term = Term(TermInfo::Branch(target), span);
                self.link(target);
            }
        } else {
            let cur = self.graph.scope(self.block.scope);
            let tscope = self.graph.scope(self.graph.block(target).scope);
            for _ in 0..cur.func_upvar_depth.strict_sub(tscope.func_upvar_depth) {
                self.block.insts.push(Inst(InstInfo::PopUpvars, span));
            }
            self.block.term = Term(TermInfo::Branch(target), span);
            self.link(target);
        }
        Ok(())
    }

    fn lower_block(&mut self, block: &'a Block, mut want_result: bool) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        // FIXME: choose better span for this
        let span = block.span();

        // Prologue

        // Push upvars if we have captures in this scope
        if scope.has_upvars() {
            self.block
                .insts
                .push(Inst(InstInfo::PushUpvars(scope.caps), span));
        }

        if self.params.is_top_level {
            if matches!(self.params.mode, Mode::Module { .. }) && scope.has_upvars() {
                want_result = false
            }

            for import in self.prelude.iter() {
                let module = match import {
                    PreludeImport::Items {
                        module,
                        items: fields,
                    } => {
                        if fields.iter().all(|f| f.res.is_none()) {
                            // Unused, skip entirely
                            continue;
                        }
                        module
                    }
                    PreludeImport::ModuleAsIs { module, res, .. }
                    | PreludeImport::ModuleRenamed { module, res, .. } => {
                        if res.is_none() {
                            // Unused, skip
                            continue;
                        }
                        module
                    }
                };
                match import {
                    PreludeImport::ModuleAsIs { res, insert, .. } => {
                        if *insert {
                            let cid = self.consttab.str(self.bintab.id_str(module));
                            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                            self.lower_load(res.as_ref().unwrap(), span);
                            let insert = self.symtab.id(&self.bintab.id_str("insert"));
                            let call = sig::Pack::new(
                                [sig::Arg::Key(insert), sig::Arg::Value].into_iter(),
                            );
                            let sig = self.packtab.id(&call);
                            self.block
                                .insts
                                .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));
                            self.block.insts.push(Inst(InstInfo::Pop, span));
                        } else {
                            let cid = self.consttab.str(self.bintab.id_str(module));
                            self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                            let module = self.symtab.id(&self.bintab.id_str("module"));
                            let call = sig::Pack::new([sig::Arg::Key(module)].into_iter());
                            let sig = self.packtab.id(&call);
                            self.block
                                .insts
                                .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));

                            self.lower_store_res(
                                res.as_ref().expect("unresolved prelude module"),
                                span,
                                false,
                            );
                        }
                    }
                    PreludeImport::ModuleRenamed { res, .. } => {
                        let cid = self.consttab.str(self.bintab.id_str(module));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        let get = self.symtab.id(&self.bintab.id_str("get"));
                        let call = sig::Pack::new([sig::Arg::Key(get)].into_iter());
                        let sig = self.packtab.id(&call);
                        self.block
                            .insts
                            .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));

                        self.lower_store_res(
                            res.as_ref().expect("unresolved prelude module"),
                            span,
                            false,
                        );
                    }
                    PreludeImport::Items { items: fields, .. } => {
                        let cid = self.consttab.str(self.bintab.id_str(module));
                        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                        let get = self.symtab.id(&self.bintab.id_str("get"));
                        let call = sig::Pack::new([sig::Arg::Key(get)].into_iter());
                        let sig = self.packtab.id(&call);
                        self.block
                            .insts
                            .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));
                        let used: Vec<_> = fields.iter().filter(|f| f.res.is_some()).collect();
                        for (i, field) in used.iter().enumerate() {
                            if i + 1 != used.len() {
                                self.block.insts.push(Inst(InstInfo::Dup, span));
                            }
                            let sym = self.symtab.id(&self.bintab.id_str(&field.item));
                            self.block.insts.push(Inst(InstInfo::Get(sym), span));
                            self.lower_store_res(
                                field.res.as_ref().expect("unresolved prelude field"),
                                span,
                                false,
                            );
                        }
                    }
                }
            }
        }

        if let Some(vars) = self.params.bind.as_ref() {
            if let Some(id) = self.params.unpack {
                self.block.insts.push(Inst(InstInfo::Unpack(id), span))
            }
            for var in vars.iter() {
                match var {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreLocal(*index), span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreUpvar(*index, *depth), span)),
                }
            }
        }

        if let Some(params) = self.params.bind_params {
            self.lower_non_const_defaults(params, span)?;
        }

        // End prologue
        for (i, stmt) in block.stmts.iter().enumerate() {
            if self.lower_stmt(stmt, want_result && i + 1 == block.stmts.len())? {
                return Ok(());
            }
        }

        if want_result && block.stmts.is_empty() {
            self.lower_load_nil(span);
        }

        // Epilogue
        if self.params.is_top_level && matches!(self.params.mode, Mode::Repl) {
            assert!(want_result);
            self.lower_store_res(block.repl.as_ref().unwrap(), span, false);
        }

        if self.params.is_top_level
            && matches!(self.params.mode, Mode::Module { .. } | Mode::Repl)
            && scope.has_upvars()
        {
            let module = sig::Pack::new(block.vars.iter().filter(|v| v.captured).map(|v| {
                if v.exported {
                    sig::Arg::Key(v.sym)
                } else {
                    sig::Arg::Value
                }
            }));
            let sig = self.packtab.id(&module);
            self.block.insts.push(Inst(InstInfo::Reify(sig), span));
        } else if scope.has_upvars() {
            self.block.insts.push(Inst(InstInfo::PopUpvars, span));
        }

        // End epilogue

        if let Some(next) = self.params.next_id {
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
        } else {
            self.block.term = Term(TermInfo::Branch(self.params.exit_id), span);
            self.link(self.params.exit_id);
        }

        Ok(())
    }

    fn lower_nl_guard_body(&mut self, stmt: &'a Stmt) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        let span = stmt.span();

        // Prologue: push upvars (NL guard scopes always have an upvar record)
        if scope.has_upvars() {
            self.block
                .insts
                .push(Inst(InstInfo::PushUpvars(scope.caps), span));
        }

        // Lower the body statement (always want result; need something to return from closure)
        if !self.lower_stmt(stmt, true)? {
            if scope.has_upvars() {
                self.block.insts.push(Inst(InstInfo::PopUpvars, span));
            }
            // Branch to exit
            self.block.term = Term(TermInfo::Branch(self.params.exit_id), span);
            self.link(self.params.exit_id);
        }

        Ok(())
    }

    fn lower_nl_guard(&mut self, guard: &'a NlGuard, want_result: bool) -> Result<bool> {
        let span = guard.span;

        // Create a zero-arg synthetic closure for the guard body
        let empty = sig::Unpack::new(0, [], [], dolang_bytecode::Variadic::None);
        let sig = self.unpacktab.id(&empty);
        let fid = self
            .graph
            .alloc_nl_guard(sig, Some(self.block.scope), self.origintab);
        let (enter, exit) = {
            let func = self.graph.func(fid);
            (func.enter, func.exit)
        };

        // Queue the body statement for lowering inside the synthetic closure.
        // Break/continue/return targets are NOT passed through — non-local jumps
        // use NlBranch which terminates via Ret.
        self.queue(Work {
            bb: enter,
            ast: WorkAst::Stmt(&guard.body),
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: None,
                break_id: None,
                break_result: false,
                continue_id: None,
                exit_id: exit,
            },
        });

        // Emit NlGuard (creates closure internally, invokes it, pushes result + indicator)
        self.block.insts.push(Inst(InstInfo::NlGuard(fid), span));

        // After NlGuard: stack = [result, indicator]
        // If indicator is falsy (nil): normal path, result is the value
        // If indicator is truthy (1/2/3): non-local jump

        // Count possible indicators to determine dispatch strategy
        let indicator_count =
            guard.has_break as u8 + guard.has_continue as u8 + guard.has_return.is_some() as u8;

        // For multi-indicator dispatch, Dup the indicator before If so it's
        // preserved on the dispatch path: stack = [result, indicator, indicator]
        if indicator_count > 1 {
            self.block.insts.push(Inst(InstInfo::Dup, span));
        }

        // TermInfo::If pops top of stack (indicator) and branches
        let normal_bb = self.graph.alloc_block(self.block.func, self.block.scope);
        let dispatch_bb = self.graph.alloc_block(self.block.func, self.block.scope);
        self.block.term = Term(TermInfo::If(dispatch_bb, normal_bb), span);
        self.link(normal_bb);
        self.link(dispatch_bb);

        // === Normal path ===
        // Single indicator: stack = [result]
        // Multi indicator: stack = [result, indicator] — pop extra indicator
        self.switch(normal_bb);
        if indicator_count > 1 {
            self.block.insts.push(Inst(InstInfo::Pop, span));
        }
        if !want_result {
            self.block.insts.push(Inst(InstInfo::Pop, span));
        }
        let after_bb = self.graph.alloc_block(self.block.func, self.block.scope);
        self.block.term = Term(TermInfo::Branch(after_bb), span);
        self.link(after_bb);

        // === Dispatch path ===
        self.switch(dispatch_bb);

        if indicator_count == 1 {
            // Single indicator: stack = [result]. Indicator consumed by If.
            self.block.insts.push(Inst(InstInfo::Pop, span));
            if guard.has_break {
                if self.params.break_result {
                    self.lower_load_nil(span);
                }
                self.lower_scope_leave(self.params.break_id.expect("no break target"), span)?;
            } else if guard.has_continue {
                self.lower_scope_leave(self.params.continue_id.expect("no continue target"), span)?;
            } else {
                self.lower_load(guard.has_return.as_ref().unwrap(), span);
                self.lower_scope_leave(self.params.exit_id, span)?;
            }
        } else {
            // Multi indicator: stack = [result, indicator].
            // Cascade-test indicator values. Indicators: 1=break, 2=continue, 3=return.
            // Build list of (indicator_value, action) pairs for present indicators.
            let mut branches: Vec<(i64, u8)> = Vec::new(); // (value, action_code)
            if guard.has_break {
                branches.push((1, 1)); // 1 = break
            }
            if guard.has_continue {
                branches.push((2, 2)); // 2 = continue
            }
            if guard.has_return.is_some() {
                branches.push((3, 3)); // 3 = return
            }

            for (i, &(indicator_val, action)) in branches.iter().enumerate() {
                let is_last = i == branches.len() - 1;

                if !is_last {
                    // Test: Dup indicator, load constant, Eq, If
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                    let cid = self.consttab.int(indicator_val.into());
                    self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                    self.block.insts.push(Inst(InstInfo::Eq, span));

                    let match_bb = self.graph.alloc_block(self.block.func, self.block.scope);
                    let next_bb = self.graph.alloc_block(self.block.func, self.block.scope);
                    self.block.term = Term(TermInfo::If(match_bb, next_bb), span);
                    self.link(match_bb);
                    self.link(next_bb);

                    // Match block: pop indicator, then handle action
                    self.switch(match_bb);
                    self.block.insts.push(Inst(InstInfo::Pop, span)); // pop indicator
                    self.lower_nl_dispatch_action(guard, action, span)?;

                    // Continue cascade in next block
                    self.switch(next_bb);
                } else {
                    // Last branch: unconditionally handle (pop indicator first)
                    self.block.insts.push(Inst(InstInfo::Pop, span)); // pop indicator
                    self.lower_nl_dispatch_action(guard, action, span)?;
                }
            }
        }

        // Continue after the guard
        self.switch(after_bb);
        Ok(false)
    }

    /// Emit the action for a single NL dispatch branch.
    /// `action`: 1=break, 2=continue, 3=return.
    /// Stack on entry: [result]. Result is NIL from NlGuard dispatch.
    fn lower_nl_dispatch_action(
        &mut self,
        guard: &'a NlGuard,
        action: u8,
        span: Span,
    ) -> Result<()> {
        match action {
            1 => {
                // break: pop NIL result, push break value if needed
                self.block.insts.push(Inst(InstInfo::Pop, span));
                if self.params.break_result {
                    self.lower_load_nil(span);
                }
                self.lower_scope_leave(self.params.break_id.expect("no break target"), span)?;
            }
            2 => {
                // continue: pop NIL result
                self.block.insts.push(Inst(InstInfo::Pop, span));
                self.lower_scope_leave(self.params.continue_id.expect("no continue target"), span)?;
            }
            3 => {
                // return: pop NIL result, load return value from upvar
                self.block.insts.push(Inst(InstInfo::Pop, span));
                self.lower_load(guard.has_return.as_ref().unwrap(), span);
                self.lower_scope_leave(self.params.exit_id, span)?;
            }
            _ => unreachable!("invalid NL dispatch action"),
        }
        Ok(())
    }

    fn lower_prim_stmt(&mut self, stmt: &'a PrimStmt, want_result: bool) -> Result<bool> {
        match stmt {
            PrimStmt::Expr(cmd) => {
                self.lower_expr(cmd)?;
                if !want_result {
                    self.block.insts.push(Inst(InstInfo::Pop, cmd.span()));
                }
                Ok(false)
            }
            PrimStmt::If(node) => {
                self.lower_if(node, want_result)?;
                Ok(false)
            }
            PrimStmt::Try(node) => {
                self.lower_try(node, want_result)?;
                Ok(false)
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &'a Stmt, want_result: bool) -> Result<bool> {
        match stmt {
            Stmt::Assign(node) => {
                self.lower_assign(node, want_result)?;
                Ok(false)
            }
            Stmt::Bind(node) => {
                self.lower_bind(node, want_result)?;
                Ok(false)
            }
            Stmt::Break(span, None) => {
                if self.params.break_result {
                    self.lower_load_nil(*span);
                }
                self.lower_scope_leave(self.params.break_id.expect("no break target"), *span)?;
                Ok(true)
            }
            Stmt::Class(class) => {
                self.lower_class(class, want_result)?;
                Ok(false)
            }
            Stmt::Break(span, Some(nl)) => {
                let ud = self.scope_to_upvar_depth(nl.scope_depth);
                self.block.term = Term(TermInfo::NlBranch(ud, nl.indicator), *span);
                Ok(true)
            }
            Stmt::Continue(span, None) => {
                self.lower_scope_leave(
                    self.params.continue_id.expect("no continue target"),
                    *span,
                )?;
                Ok(true)
            }
            Stmt::Continue(span, Some(nl)) => {
                let ud = self.scope_to_upvar_depth(nl.scope_depth);
                self.block.term = Term(TermInfo::NlBranch(ud, nl.indicator), *span);
                Ok(true)
            }
            Stmt::Def(node) => {
                self.lower_def(node, want_result)?;
                Ok(false)
            }
            Stmt::For(node) => {
                self.lower_for(node, want_result)?;
                Ok(false)
            }
            Stmt::Import(import) => {
                self.lower_import(import, want_result)?;
                Ok(false)
            }
            Stmt::Let(node) => {
                self.lower_let(node, want_result)?;
                Ok(false)
            }
            Stmt::NlGuard(guard) => self.lower_nl_guard(guard, want_result),
            Stmt::Prim(prim) => self.lower_prim_stmt(prim, want_result),
            Stmt::Return(Return {
                expr,
                span,
                nl: None,
            }) => {
                if let Some(expr) = expr {
                    self.lower_expr(expr)?;
                } else {
                    self.lower_load_nil(*span);
                }
                self.lower_scope_leave(self.params.exit_id, *span)?;
                Ok(true)
            }
            Stmt::Return(Return {
                expr,
                span,
                nl: Some(nl),
            }) => {
                if let Some(expr) = expr {
                    self.lower_expr(expr)?;
                } else {
                    self.lower_load_nil(*span);
                }
                // Store return value into the synthetic upvar
                let ret_upvar = nl.ret_upvar.as_ref().expect("nl return missing ret_upvar");
                self.lower_store_res(ret_upvar, *span, false);
                let ud = self.scope_to_upvar_depth(nl.scope_depth);
                self.block.term = Term(TermInfo::NlBranch(ud, nl.indicator), *span);
                Ok(true)
            }
            Stmt::Throw(node) => {
                self.lower_expr(&node.expr)?;
                let sig = sig::Pack::new(std::iter::once(sig::Arg::Value));
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::THROW, self.packtab.id(&sig)),
                    node.span,
                ));
                self.lower_scope_leave(self.params.exit_id, node.span)?;
                Ok(true)
            }
            Stmt::While(node) => {
                self.lower_while(node, want_result)?;
                Ok(false)
            }
        }
    }

    fn lower_function(&mut self, function: &'a Function, sig: sig::UnpackId) -> Result<()> {
        // Compute order in which to move arguments into locals or upvars
        self.params.bind = Some(self.unpack_order(&function.params, sig));
        self.params.bind_params = Some(&function.params);
        self.lower_block(&function.body, true)?;
        Ok(())
    }

    fn unpack_order_in_scope(
        &mut self,
        scope: cfg::ScopeRef<'a>,
        params: &[Param],
        sig: sig::UnpackId,
    ) -> Vec<Var> {
        let pos: Vec<_> = params
            .iter()
            .filter_map(|p| {
                if let Param::Pos { ident, .. } = p {
                    Some(ident)
                } else {
                    None
                }
            })
            .collect();
        let mut sym_keys: Vec<_> = params
            .iter()
            .filter_map(|p| {
                if let Param::Key {
                    key_span, ident, ..
                } = p
                {
                    Some((
                        self.symtab
                            .id(&self.bintab.id_str(self.file.str(*key_span))),
                        ident,
                    ))
                } else {
                    None
                }
            })
            .collect();
        let mut const_keys: Vec<_> = params
            .iter()
            .filter_map(|p| {
                if let Param::ConstKey {
                    key_const, ident, ..
                } = p
                {
                    Some((self.lower_const(key_const), ident))
                } else {
                    None
                }
            })
            .collect();
        let rest = params.last().and_then(|p| {
            if let Param::Rest { ident, .. } = p {
                ident.as_ref()
            } else {
                None
            }
        });
        sym_keys.sort_by_key(|(sym, _)| *sym);
        const_keys.sort_by_key(|(c, _)| *c);
        let unpack = &self.unpacktab[sig];
        let mut vars = Vec::new();
        if let Some(id) = rest {
            let res = id.res.as_ref().expect("unresolved param");
            let var = self.resolve_var_in_scope(cfg::ScopeRef::clone(&scope), res.index, res.depth);
            vars.push(var);
        }
        for unpack_key in unpack.iter_keys().rev() {
            let ident = match &unpack_key.kind {
                sig::UnpackKeyKind::Sym(sym) => {
                    let index = sym_keys
                        .binary_search_by_key(sym, |(s, _)| *s)
                        .expect("key symbol not in parameters?!");
                    sym_keys[index].1
                }
                sig::UnpackKeyKind::Const(c) => {
                    let index = const_keys
                        .binary_search_by_key(c, |(const_id, _)| *const_id)
                        .expect("constant key not in parameters?!");
                    const_keys[index].1
                }
            };
            let res = ident.res.as_ref().expect("unresolved param");
            let var = self.resolve_var_in_scope(cfg::ScopeRef::clone(&scope), res.index, res.depth);
            vars.push(var);
        }
        for id in pos.iter().rev() {
            let res = id.res.as_ref().expect("unresolved param");
            let var = self.resolve_var_in_scope(cfg::ScopeRef::clone(&scope), res.index, res.depth);
            vars.push(var);
        }
        vars
    }

    fn unpack_order(&mut self, params: &[Param], sig: sig::UnpackId) -> Vec<Var> {
        self.unpack_order_in_scope(self.graph.scope(self.block.scope), params, sig)
    }

    fn lower_for_args(&mut self, body: &'a [Arg]) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        // FIXME: choose better span for this
        let span = body.span();

        // Prologue

        // Push upvars if we have captures in this scope
        if scope.has_upvars() {
            self.block
                .insts
                .push(Inst(InstInfo::PushUpvars(scope.caps), span));
        }

        if let Some(vars) = self.params.bind.as_ref() {
            if let Some(id) = self.params.unpack {
                self.block.insts.push(Inst(InstInfo::Unpack(id), span))
            }
            for var in vars.iter() {
                match var {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreLocal(*index), span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreUpvar(*index, *depth), span)),
                }
            }
        }

        if let Some(params) = self.params.bind_params {
            self.lower_non_const_defaults(params, span)?;
        }

        let mut sig = vec![sig::Arg::Pack];
        // End prologue
        for arg in body.iter() {
            sig.push(self.lower_arg(arg)?);
        }
        let sig = sig::Pack::new(sig.into_iter());
        let sig = self.packtab.id(&sig);

        self.block
            .insts
            .push(Inst(InstInfo::Builtin(builtin::ARGS, sig), span));

        // Epilogue
        if scope.has_upvars() {
            self.block.insts.push(Inst(InstInfo::PopUpvars, span));
        }
        // End epilogue

        if let Some(next) = self.params.next_id {
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
        } else {
            unreachable!();
        }

        Ok(())
    }

    fn lower_for_array(&mut self, body: &'a [ArrayElem]) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        // FIXME: choose better span for this
        let span = body.span();

        // Prologue

        // Push upvars if we have captures in this scope
        if scope.has_upvars() {
            self.block
                .insts
                .push(Inst(InstInfo::PushUpvars(scope.caps), span));
        }

        if let Some(vars) = self.params.bind.as_ref() {
            if let Some(id) = self.params.unpack {
                self.block.insts.push(Inst(InstInfo::Unpack(id), span))
            }
            for var in vars.iter() {
                match var {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreLocal(*index), span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreUpvar(*index, *depth), span)),
                }
            }
        }

        if let Some(params) = self.params.bind_params {
            self.lower_non_const_defaults(params, span)?;
        }

        let mut sig = vec![sig::Arg::Pack];
        // End prologue
        for elem in body.iter() {
            sig.push(self.lower_array_elem(elem)?);
        }
        let sig = sig::Pack::new(sig.into_iter());
        let sig = self.packtab.id(&sig);

        self.block
            .insts
            .push(Inst(InstInfo::Builtin(builtin::ARGS, sig), span));

        // Epilogue
        if scope.has_upvars() {
            self.block.insts.push(Inst(InstInfo::PopUpvars, span));
        }
        // End epilogue

        if let Some(next) = self.params.next_id {
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
        } else {
            unreachable!();
        }

        Ok(())
    }

    fn lower_for_dict(&mut self, body: &'a [DictElem]) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        // FIXME: choose better span for this
        let span = body.span();

        // Prologue

        // Push upvars if we have captures in this scope
        if scope.has_upvars() {
            self.block
                .insts
                .push(Inst(InstInfo::PushUpvars(scope.caps), span));
        }

        if let Some(vars) = self.params.bind.as_ref() {
            if let Some(id) = self.params.unpack {
                self.block.insts.push(Inst(InstInfo::Unpack(id), span))
            }
            for var in vars.iter() {
                match var {
                    Var::Local(index) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreLocal(*index), span)),
                    Var::Upvar(index, depth) => self
                        .block
                        .insts
                        .push(Inst(InstInfo::StoreUpvar(*index, *depth), span)),
                }
            }
        }

        if let Some(params) = self.params.bind_params {
            self.lower_non_const_defaults(params, span)?;
        }

        let mut sig = vec![sig::Arg::Pack];
        // End prologue
        for elem in body.iter() {
            let (arg1, arg2) = self.lower_dict_elem(elem)?;
            sig.push(arg1);
            if let Some(arg2) = arg2 {
                sig.push(arg2)
            }
        }
        let sig = sig::Pack::new(sig.into_iter());
        let sig = self.packtab.id(&sig);

        self.block
            .insts
            .push(Inst(InstInfo::Builtin(builtin::ARGS, sig), span));

        // Epilogue
        if scope.has_upvars() {
            self.block.insts.push(Inst(InstInfo::PopUpvars, span));
        }
        // End epilogue

        if let Some(next) = self.params.next_id {
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
        } else {
            unreachable!();
        }

        Ok(())
    }
}

impl<'c> Lowerer<'c> {
    fn denormalize(&mut self, graph: &mut cfg::Graph) {
        for bid in graph.iter_blocks() {
            let block = graph.block(bid);
            let sid = if let cfg::Term(TermInfo::Branch(sid), _) = &block.term {
                *sid
            } else {
                continue;
            };
            drop(block);
            let sblock = graph.block(sid);
            if !sblock.insts.is_empty() || !matches!(sblock.term.0, TermInfo::Ret) {
                continue;
            }
            drop(sblock);
            let mut sblock = graph.block_mut(sid);
            sblock.inbound.remove(&bid);
            drop(sblock);
            let mut block = graph.block_mut(bid);
            block.term.0 = TermInfo::Ret;
        }
    }

    pub(crate) fn run(&mut self, root: &Unit) -> Result<cfg::Graph> {
        let mut graph = cfg::Graph::new();
        let empty = sig::Unpack::new(0, [], [], dolang_bytecode::Variadic::None);
        let sig = self.unpacktab.id(&empty);
        let fid = graph.alloc_func(sig, None, &root.0.body.vars, None, self.origintab);
        let (enter, exit) = {
            let func = graph.func(fid);
            (func.enter, func.exit)
        };
        let mut queue = Queue::new();
        queue.push(Work {
            ast: WorkAst::Function(&root.0, sig),
            bb: enter,
            params: Params {
                bind: None,
                bind_params: None,
                unpack: None,
                mode: self.mode.clone(),
                is_top_level: true,
                next_id: None,
                continue_id: None,
                break_id: None,
                break_result: false,
                exit_id: exit,
            },
        });
        while let Some(work) = queue.pop() {
            let mut scope = Scope {
                file: self.file,
                symtab: self.symtab,
                bintab: self.bintab,
                consttab: self.consttab,
                packtab: self.packtab,
                unpacktab: self.unpacktab,
                prelude: self.prelude,
                origintab: self.origintab,
                sentinel_const: &mut self.sentinel_const,
                graph: &graph,
                bb: work.bb,
                block: graph.block_mut(work.bb),
                params: work.params,
                queue: &mut queue,
            };
            match work.ast {
                WorkAst::Function(function, sig) => scope.lower_function(function, sig)?,
                WorkAst::ClassBody(block) => scope.lower_class_body(block)?,
                WorkAst::Block(block, want_result) => scope.lower_block(block, want_result)?,
                WorkAst::Stmt(stmt) => scope.lower_nl_guard_body(stmt)?,
                WorkAst::Args(body) => scope.lower_for_args(body)?,
                WorkAst::ArrayElems(body) => scope.lower_for_array(body)?,
                WorkAst::DictElems(body) => scope.lower_for_dict(body)?,
            }
        }
        mem::drop(queue);
        self.denormalize(&mut graph);
        Ok(graph)
    }
}
