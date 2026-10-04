use std::{cell::OnceCell, mem, str::Utf8Error};

use dolang_util::mono::MonoVec;

use dolang_bytecode::builtin;

use crate::{
    Mode, PreludeImport, RestKind,
    ast::{
        Arg, Arm, ArrayElem, Assign, Bind, Block, Class, ClassMember, ClassSuper, CondPattern,
        Const, Decorator, Def, DictElem, Expand, Expr, ExprBody, FieldInit, FmtParamName, For,
        FormatAlign, FormatKind, FormatSign, FormatSpec, Function, GetVariant, Guard, Ident, If,
        Import, ImportElement, ImportItem, Key, LValue, Let, Match, MemberScope, Method, NlGuard,
        Pair, PatBind, PatDefault, PatIdent, PatItem, Pattern, PrimStmt, Res, Return, Root, Single,
        Stmt, Try, While, visit::Node,
    },
    cfg::{self, BlockRefMut, Inst, InstInfo, Term, TermInfo},
    constant::{self, ConstantExt},
    intern,
    lex::Op,
    sig,
    source::{File, Span},
    sym,
};

pub(crate) struct Lowerer<'c> {
    pub(crate) mode: Mode<'c>,
    pub(crate) file: &'c File<'c>,
    pub(crate) symtab: &'c sym::Table,
    pub(crate) bintab: &'c intern::BinTable,
    pub(crate) consttab: &'c constant::Table,
    pub(crate) packtab: &'c sig::PackTable,
    pub(crate) unpacktab: &'c sig::UnpackTable,
    pub(crate) prelude: &'c [PreludeImport],
    pub(crate) sentinel_const: OnceCell<constant::Id>,
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct Error {}

impl From<Utf8Error> for Error {
    fn from(_value: Utf8Error) -> Self {
        Self {}
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Var {
    Local(usize),
    Upvar(usize, usize),
}

/// What carries the non-constant defaults a prologue evaluates once its values
/// are bound
#[derive(Clone, Copy)]
enum Defaults<'a> {
    /// A function's parameters
    Items(&'a [PatItem]),
    Pattern(&'a Pattern),
}

/// The prologue bindings a branch body needs: how to bind the values the
/// terminator left on the operand stack, and the pattern carrying any
/// non-constant defaults.
type Binds<'a> = (Option<BindPlan>, Option<Defaults<'a>>);

/// How to bind the values an unpack leaves on the operand stack.
///
/// Sub-patterns unpack in turn, each leaving its own values in place of the value
/// it matched, until only bindings and values to discard remain. Tests check a
/// copy of a value, which stays in place for its inner pattern.
struct BindPlan {
    /// The constant tests, class tests and sub-pattern unpacks, in order
    steps: Vec<BindStep>,
    /// The destinations of remaining values, from the top; `None` discards a value
    vars: Vec<Option<Var>>,
}

/// A test or unpack of a sub-pattern's value, after preceding steps.
struct BindStep {
    /// The value's depth on the operand stack, counting from the top
    depth: usize,
    op: BindOp,
    /// The values a failed match must discard: the others on the stack, and a
    /// tested value
    others: usize,
}

enum BindOp {
    Constant(constant::Id),
    Unpack(sig::UnpackId),
    TypeTest {
        var: Var,
        fields: Vec<sym::Id>,
    },
    /// Alternatives, each matching the value on its own, which leave the values
    /// of `canon`, and above them `indicator`'s, in place of the value
    Alt {
        alts: Vec<BindPlan>,
        /// Every variable an alternative binds, from the top
        canon: Vec<Var>,
        indicator: Option<Var>,
    },
}

/// Where a failed match continues
#[derive(Clone, Copy)]
enum Fail {
    /// Nowhere: the match raises
    Raise,
    /// At `target`, after discarding the values the plan left, and `below` more
    Goto { target: cfg::BlockId, below: usize },
}

/// A value an unpack leaves on the operand stack
enum Slot<'a> {
    Discard,
    Var(Var),
    /// The value of a sub-pattern
    Pattern(&'a Pattern),
}

struct Params<'a> {
    bind: Option<BindPlan>,
    bind_params: Option<Defaults<'a>>,
    mode: Mode<'a>,
    is_top_level: bool,
    next_id: Option<cfg::BlockId>,
    break_id: Option<cfg::BlockId>,
    break_result: bool,
    continue_id: Option<cfg::BlockId>,
    exit_id: cfg::BlockId,
}

enum WorkAst<'a> {
    Function(&'a Function, sig::UnpackId),
    Block(&'a Block, bool),
    /// A `match` arm's body, with the block its guard fails to
    Arm(&'a Arm, bool, cfg::BlockId),
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

type Queue<'a> = MonoVec<Work<'a>>;

struct Scope<'a, 'c, 'q> {
    file: &'c File<'c>,
    symtab: &'c sym::Table,
    bintab: &'c intern::BinTable,
    consttab: &'c constant::Table,
    packtab: &'c sig::PackTable,
    unpacktab: &'c sig::UnpackTable,
    prelude: &'c [PreludeImport],
    sentinel_const: &'c OnceCell<constant::Id>,
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
            .filter(|(_, v)| v.is_emitted())
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
                        builtin::CONCAT_VERBATIM
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

    /// Lowers a `t"..."` sequence: one argument per segment.
    ///
    /// A run of literal text folds to a single constant, exactly as
    /// concatenation folds one, so the segments a consumer sees are the
    /// literal runs and the interpolations between them.
    fn lower_fmt_seq(&mut self, exprs: &'a [Expr], span: Span) -> Result<()> {
        let mut acc = String::new();
        let mut count = 0;

        for expr in exprs.iter() {
            match expr {
                Expr::Literal(span) => acc.push_str(self.file.str(*span)),
                Expr::Escape(char, _) => acc.push(*char),
                other => {
                    if !acc.is_empty() {
                        self.lower_fmt_seq_literal(&acc, span);
                        acc.clear();
                        count += 1;
                    }
                    self.lower_expr(other)?;
                    if !matches!(other, Expr::Fmt { .. } | Expr::FmtParam { .. }) {
                        // An interpolation stating no specification is an
                        // interpolation all the same: bind it, so every
                        // segment is either literal text or a bound value.
                        self.lower_bare_interp(other.span());
                    }
                    count += 1;
                }
            }
        }
        if !acc.is_empty() {
            self.lower_fmt_seq_literal(&acc, span);
            count += 1;
        }

        let sig = sig::Pack::new(vec![sig::Arg::Value; count].into_iter());
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::FMT, self.packtab.id(&sig)),
            span,
        ));
        Ok(())
    }

    /// Binds the value just lowered to an empty specification, recording the
    /// text it was written as.
    fn lower_bare_interp(&mut self, span: Span) {
        let cid = self.consttab.str(self.bintab.id_str(self.file.str(span)));
        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
        let sig = sig::Pack::new(
            [
                sig::Arg::Value,
                sig::Arg::Key(self.symtab.id(&self.bintab.id_str("source"))),
            ]
            .into_iter(),
        );
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::FMT_VALUE, self.packtab.id(&sig)),
            span,
        ));
    }

    fn lower_fmt_seq_literal(&mut self, text: &str, span: Span) {
        let cid = self.consttab.str(self.bintab.id_str(text));
        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
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
                Expr::Escape(char, _) => {
                    let mut buf = [0; 4];
                    acc.extend_from_slice(char.encode_utf8(&mut buf).as_bytes());
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
                verbatim,
            } => self.lower_concat(exprs, *delim_span, *verbatim)?,
            Expr::Fmt { value, spec, .. } => self.lower_fmt(value, spec, expr.span())?,
            Expr::FmtParam { name, spec, .. } => self.lower_fmt_param(name, spec, expr.span())?,
            Expr::Escape(char, span) => {
                let cid = self.consttab.str(self.bintab.id_str(&format!("{char}")));
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), *span));
            }
            Expr::FmtSeq { exprs, open, close } => {
                let span = close.map_or(*open, |close| *open | close);
                self.lower_fmt_seq(exprs, span)?;
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
            Expr::Stub(span) => {
                let sig = self.packtab.id(&sig::Pack::new(std::iter::empty()));
                self.block
                    .insts
                    .push(Inst(InstInfo::Builtin(builtin::STUB, sig), *span));
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
                        Op::Bang
                        | Op::AmpAmp
                        | Op::BarBar
                        | Op::Dot
                        | Op::DotHash
                        | Op::StarStar => {
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
            Expr::Array { elems, .. } | Expr::Tuple { elems, .. } => {
                let mut sig = Vec::new();
                for elem in elems.iter() {
                    sig.push(self.lower_array_elem(elem)?);
                }
                let sig = sig::Pack::new(sig.into_iter());
                let builtin = if matches!(expr, Expr::Tuple { .. }) {
                    builtin::TUPLE
                } else {
                    builtin::ARRAY
                };
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin, self.packtab.id(&sig)),
                    expr.span(),
                ));
            }
            Expr::Record { args, .. } => {
                let mut sig = Vec::new();
                for arg in args.iter() {
                    sig.push(self.lower_arg(arg)?);
                }
                let sig = sig::Pack::new(sig.into_iter());
                self.block.insts.push(Inst(
                    InstInfo::Builtin(builtin::RECORD, self.packtab.id(&sig)),
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

    fn lower_fmt(&mut self, value: &'a Expr, spec: &'a FormatSpec, span: Span) -> Result<()> {
        self.lower_expr(value)?;
        let pack = self.lower_fmt_spec(spec, span)?;
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::FMT_VALUE, self.packtab.id(&pack)),
            span,
        ));
        Ok(())
    }

    /// Lowers a `${#0}` or `${#foo}`: the name as the positional, then the
    /// same specification pack an interpolation builds.
    fn lower_fmt_param(
        &mut self,
        name: &'a FmtParamName,
        spec: &'a FormatSpec,
        span: Span,
    ) -> Result<()> {
        match name {
            FmtParamName::Pos(value, name_span) => {
                let cid = self.lower_const(&Const::Int(*value as i128));
                self.block
                    .insts
                    .push(Inst(InstInfo::LoadConst(cid), *name_span));
            }
            FmtParamName::Named(name_span) => {
                let id = self
                    .symtab
                    .id(&self.bintab.id_str(self.file.str(*name_span)));
                let cid = self.consttab.sym(id);
                self.block
                    .insts
                    .push(Inst(InstInfo::LoadConst(cid), *name_span));
            }
        }
        let pack = self.lower_fmt_spec(spec, span)?;
        self.block.insts.push(Inst(
            InstInfo::Builtin(builtin::FMT_PARAM, self.packtab.id(&pack)),
            span,
        ));
        Ok(())
    }

    /// Lowers a specification as keyword arguments over a value already on the
    /// stack, ending with the `source:` text, and returns the argument pack.
    fn lower_fmt_spec(&mut self, spec: &'a FormatSpec, span: Span) -> Result<sig::Pack> {
        let mut sig = vec![sig::Arg::Value];

        if let Some(fill) = &spec.fill {
            let mut buf = [0; 4];
            let value = Const::Str(fill.value.encode_utf8(&mut buf).to_owned());
            let cid = self.lower_const(&value);
            self.block
                .insts
                .push(Inst(InstInfo::LoadConst(cid), fill.span));
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("fill"))));
        }
        if let Some(align) = &spec.align {
            self.lower_known_sym(
                match align.value {
                    FormatAlign::Left => "LEFT",
                    FormatAlign::Right => "RIGHT",
                    FormatAlign::Center => "CENTER",
                },
                align.span,
            );
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("align"))));
        }
        if let Some(sign) = &spec.sign {
            self.lower_known_sym(
                match sign.value {
                    FormatSign::Plus => "PLUS",
                    FormatSign::Space => "SPACE",
                },
                sign.span,
            );
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("sign"))));
        }
        if let Some(alt_span) = spec.alt {
            let cid = self.lower_const(&Const::Bool(true));
            self.block
                .insts
                .push(Inst(InstInfo::LoadConst(cid), alt_span));
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("alt"))));
        }
        if let Some(zero_span) = spec.zero {
            self.lower_known_sym("ZERO", zero_span);
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("fill"))));
        }
        for (count, key) in [
            (spec.width.as_ref(), "width"),
            (spec.precision.as_ref(), "precision"),
        ] {
            if let Some(count) = count {
                self.lower_expr(count)?;
                sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str(key))));
            }
        }
        if let Some(kind) = &spec.kind {
            self.lower_known_sym(
                match kind.value {
                    FormatKind::Str => "STR",
                    FormatKind::Dbg => "DBG",
                    FormatKind::Verbatim => "VERBATIM",
                    FormatKind::Hex => "HEX",
                    FormatKind::Oct => "OCT",
                    FormatKind::Bin => "BIN",
                    FormatKind::Dec => "DEC",
                    FormatKind::Exp => "EXP",
                    FormatKind::Fixed => "FIXED",
                },
                kind.span,
            );
            sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("kind"))));
        }
        // The interpolation keeps the text it was written as, sigil and
        // delimiters included, so a consumer can reproduce the source form.
        let cid = self.consttab.str(self.bintab.id_str(self.file.str(span)));
        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
        sig.push(sig::Arg::Key(self.symtab.id(&self.bintab.id_str("source"))));
        Ok(sig::Pack::new(sig.into_iter()))
    }

    fn lower_known_sym(&mut self, value: &str, span: Span) {
        let id = self.symtab.id(&self.bintab.id_str(value));
        let cid = self.consttab.sym(id);
        self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
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
                self.lower_elem_if(node, WorkAst::DictElems)?;
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
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let binds = self.pattern_plan(self.graph.scope(bscope), bind)?;
                let bind_params = Some(Defaults::Pattern(bind));
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::DictElems(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
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
                self.lower_elem_if(node, WorkAst::ArrayElems)?;
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
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let binds = self.pattern_plan(self.graph.scope(bscope), bind)?;
                let bind_params = Some(Defaults::Pattern(bind));
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::ArrayElems(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
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
                );
                let bodyid = self.graph.alloc_block(self.block.func, bscope);
                let binds = self.pattern_plan(self.graph.scope(bscope), bind)?;
                let bind_params = Some(Defaults::Pattern(bind));
                // FIXME: include span of keyword, not of block
                self.queue(Work {
                    bb: bodyid,
                    ast: WorkAst::Args(&body.elems),
                    params: Params {
                        bind: Some(binds),
                        bind_params,
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
                self.lower_elem_if(node, WorkAst::Args)?;
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
        let span = bind.span();
        if want_result {
            self.block.insts.push(Inst(InstInfo::Dup, span));
        }
        let plan = self.pattern_plan(self.graph.scope(self.block.scope), bind)?;
        self.lower_bind_plan(plan, span);
        self.lower_pattern_defaults(bind, span)?;

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

    /// Lower the test of a conditional branch and set the current block's terminator.
    ///
    /// `bscope` is the scope allocated for the branch body, `tid` the block the body
    /// starts in, and `fid` the block reached when the test fails.  Returns the `bind`
    /// and `bind_params` the body's prologue needs. Tests and unpacks each end a
    /// block, with failure edges discarding the values left by earlier steps.
    fn lower_cond(
        &mut self,
        cond: &'a Expr,
        bind: Option<&'a CondPattern>,
        bscope: cfg::ScopeId,
        tid: cfg::BlockId,
        fid: cfg::BlockId,
        span: Span,
    ) -> Result<Binds<'a>> {
        self.lower_expr(cond)?;
        let Some(bind) = bind else {
            self.block.term = Term(TermInfo::If(tid, fid), span);
            self.link(tid);
            self.link(fid);
            return Ok((None, None));
        };
        self.lower_cond_test(&bind.pattern, true, bscope, tid, fid, span)
    }

    /// Lower the test of a pattern against the value on top of the stack, as
    /// [`Self::lower_cond`] does.  With `truthy`, a bare name tests the value's
    /// truthiness, as in `if let`; otherwise it always matches.
    fn lower_cond_test(
        &mut self,
        pattern: &'a Pattern,
        truthy: bool,
        bscope: cfg::ScopeId,
        tid: cfg::BlockId,
        fid: cfg::BlockId,
        span: Span,
    ) -> Result<Binds<'a>> {
        match pattern {
            Pattern::Ident(PatIdent { ident, .. }) if truthy => {
                // A bare identifier binds the scrutinee itself and branches on its
                // truthiness, so the value has to survive the test.  That leaves the
                // duplicate on the failure edge, which needs a block of its own to drop
                // it: the real failure target is shared with predecessors that have no
                // such value to clean up.
                let cleanup = self.graph.alloc_block(self.block.func, self.block.scope);
                self.block.insts.push(Inst(InstInfo::Dup, span));
                self.block.term = Term(TermInfo::If(tid, cleanup), span);
                self.link(tid);
                self.link(cleanup);
                let test = self.bb;
                self.switch(cleanup);
                self.block.insts.push(Inst(InstInfo::Pop, span));
                self.block.term = Term(TermInfo::Branch(fid), span);
                self.link(fid);
                self.switch(test);
                let res = ident.res.as_ref().expect("unresolved pattern binding");
                let var = self.resolve_var_in_scope(self.graph.scope(bscope), res.index, res.depth);
                Ok((
                    Some(BindPlan {
                        steps: Vec::new(),
                        vars: vec![Some(var)],
                    }),
                    None,
                ))
            }
            pattern => {
                let BindPlan { steps, vars } =
                    self.pattern_plan(self.graph.scope(bscope), pattern)?;
                let test = self.bb;
                let fail = Fail::Goto {
                    target: fid,
                    below: 0,
                };
                self.lower_steps(steps, fail, span);
                self.block.term = Term(TermInfo::Branch(tid), span);
                self.link(tid);
                self.switch(test);
                Ok((
                    Some(BindPlan {
                        steps: Vec::new(),
                        vars,
                    }),
                    Some(Defaults::Pattern(pattern)),
                ))
            }
        }
    }

    /// Lower an `if` in vertical-element layout, where each branch body is a list
    /// of arguments, array elements, or dict elements rather than a block.
    ///
    /// Mirrors [`Self::lower_if`], differing only in what the branches contribute
    /// to: an empty `ARGS` builtin opens the pack each body pushes into, and
    /// `work_ast` wraps the body for the queue.  There is no result value, so no
    /// `want_result` distinction.
    fn lower_elem_if<T>(
        &mut self,
        node: &'a If<ExprBody<T>>,
        work_ast: fn(&'a [T]) -> WorkAst<'a>,
    ) -> Result<()> {
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

        // Build the control flow structure from the inside out
        let mut fallback = next;

        // Handle final else branch if present
        if let Some((else_body, _)) = &node.else_branch {
            let fscope = self.graph.alloc_scope(
                false,
                false,
                self.block.func,
                Some(self.block.scope),
                &else_body.vars,
            );
            fallback = self.graph.alloc_block(self.block.func, fscope);
            self.queue(Work {
                bb: fallback,
                ast: work_ast(&else_body.elems),
                params: Params {
                    bind: None,
                    bind_params: None,
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
            );
            let tid = self.graph.alloc_block(self.block.func, tscope);

            self.switch(fallback);
            let (bind, bind_params) = self.lower_cond(
                &elif_branch.expr,
                elif_branch.bind.as_ref(),
                tscope,
                tid,
                current_fallback,
                elif_branch.span,
            )?;
            self.queue(Work {
                bb: tid,
                ast: work_ast(&elif_branch.body.elems),
                params: Params {
                    bind,
                    bind_params,
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

        // Finally, process the initial if branch
        self.switch(start);
        let current_fallback = fallback;
        let tscope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.tbranch.body.vars,
        );
        let tid = self.graph.alloc_block(self.block.func, tscope);
        let (bind, bind_params) = self.lower_cond(
            &node.tbranch.expr,
            node.tbranch.bind.as_ref(),
            tscope,
            tid,
            current_fallback,
            node.tbranch.span,
        )?;
        self.queue(Work {
            bb: tid,
            ast: work_ast(&node.tbranch.body.elems),
            params: Params {
                bind,
                bind_params,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(next),
                break_id: self.params.break_id,
                break_result: self.params.break_result,
                continue_id: self.params.continue_id,
                exit_id: self.params.exit_id,
            },
        });

        self.switch(next);
        Ok(())
    }

    /// Lower a `match`.  The scrutinee is stored once, and each arm loads it to
    /// test its pattern, failing over to the next arm, then `else`.
    fn lower_match(&mut self, node: &'a Match, want_result: bool) -> Result<()> {
        let span = node.match_span;
        let scrutinee = Res {
            index: node.var.expect("unelaborated match"),
            depth: 0,
            node: None,
        };
        self.lower_expr(&node.scrutinee)?;
        self.lower_store_res(&scrutinee, span, false);

        let next = self.graph.alloc_block(self.block.func, self.block.scope);
        let start = self.bb;
        let mut fallback = next;

        if let Some((else_block, _)) = &node.else_branch {
            let fscope = self.graph.alloc_scope(
                false,
                false,
                self.block.func,
                Some(self.block.scope),
                &else_block.vars,
            );
            fallback = self.graph.alloc_block(self.block.func, fscope);
            self.queue(Work {
                bb: fallback,
                ast: WorkAst::Block(else_block, want_result),
                params: self.branch_params(None, None, next),
            });
        } else if want_result {
            fallback = self.graph.alloc_block(self.block.func, self.block.scope);
            self.switch(fallback);
            self.lower_load_nil(span);
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
        }

        // Build the arms from the last to the first, each failing over to the next
        for arm in node.arms.iter().rev() {
            let current_fallback = fallback;
            fallback = self.graph.alloc_block(self.block.func, self.block.scope);
            let tscope = self.graph.alloc_scope(
                false,
                false,
                self.block.func,
                Some(self.block.scope),
                &arm.body.vars,
            );
            let tid = self.graph.alloc_block(self.block.func, tscope);
            self.switch(fallback);
            let span = arm.pattern.span();
            self.lower_load(&scrutinee, span);
            let (bind, bind_params) =
                self.lower_cond_test(&arm.pattern, false, tscope, tid, current_fallback, span)?;
            self.queue(Work {
                bb: tid,
                ast: WorkAst::Arm(arm, want_result, current_fallback),
                params: self.branch_params(bind, bind_params, next),
            });
        }

        self.switch(start);
        self.block.term = Term(TermInfo::Branch(fallback), span);
        self.link(fallback);
        self.switch(next);
        Ok(())
    }

    /// The parameters of a branch body queued from the current block, which
    /// continues at `next`
    fn branch_params(
        &self,
        bind: Option<BindPlan>,
        bind_params: Option<Defaults<'a>>,
        next: cfg::BlockId,
    ) -> Params<'a> {
        Params {
            bind,
            bind_params,
            mode: self.params.mode.clone(),
            is_top_level: false,
            next_id: Some(next),
            break_id: self.params.break_id,
            break_result: self.params.break_result,
            continue_id: self.params.continue_id,
            exit_id: self.params.exit_id,
        }
    }

    /// Lower a `match` arm's guard in its body's prologue, after the arm's
    /// bindings.  On failure, leave the arm's scope for `fail`; on success, bind
    /// the guard's own pattern, if it has one.
    fn lower_guard(&mut self, guard: &'a Guard, fail: cfg::BlockId, span: Span) -> Result<()> {
        let leave = self.graph.alloc_block(self.block.func, self.block.scope);
        let pass = self.graph.alloc_block(self.block.func, self.block.scope);
        let (bind, bind_params) = self.lower_cond(
            &guard.expr,
            guard.bind.as_ref(),
            self.block.scope,
            pass,
            leave,
            guard.if_span,
        )?;

        self.switch(leave);
        if self.graph.scope(self.block.scope).has_upvars() {
            self.block.insts.push(Inst(InstInfo::PopUpvars, span));
        }
        self.block.term = Term(TermInfo::Branch(fail), span);
        self.link(fail);

        self.switch(pass);
        self.params.bind = bind;
        self.params.bind_params = bind_params;
        self.lower_prologue_bind(span)
    }

    fn lower_if(&mut self, node: &'a If<Block>, want_result: bool) -> Result<()> {
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
            );
            fallback = self.graph.alloc_block(self.block.func, fscope);
            self.queue(Work {
                bb: fallback,
                ast: WorkAst::Block(else_block, want_result),
                params: Params {
                    bind: None,
                    bind_params: None,
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

        if node.else_branch.is_none() && want_result {
            fallback = self.graph.alloc_block(self.block.func, self.block.scope);
            self.switch(fallback);
            self.lower_load_nil(node.span());
            self.block.term = Term(TermInfo::Branch(next), node.span());
            self.link(next);
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
            );
            let tid = self.graph.alloc_block(self.block.func, tscope);

            self.switch(fallback);
            let (bind, bind_params) = self.lower_cond(
                &elif_branch.expr,
                elif_branch.bind.as_ref(),
                tscope,
                tid,
                current_fallback,
                elif_branch.span,
            )?;
            self.queue(Work {
                bb: tid,
                ast: WorkAst::Block(&elif_branch.body, want_result),
                params: Params {
                    bind,
                    bind_params,
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

        // Finally, process the initial if branch
        self.switch(start);
        let current_fallback = fallback;
        let tscope = self.graph.alloc_scope(
            false,
            false,
            self.block.func,
            Some(self.block.scope),
            &node.tbranch.body.vars,
        );
        let tid = self.graph.alloc_block(self.block.func, tscope);
        let (bind, bind_params) = self.lower_cond(
            &node.tbranch.expr,
            node.tbranch.bind.as_ref(),
            tscope,
            tid,
            current_fallback,
            node.tbranch.span,
        )?;
        self.queue(Work {
            bb: tid,
            ast: WorkAst::Block(&node.tbranch.body, want_result),
            params: Params {
                bind,
                bind_params,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(next),
                break_id: self.params.break_id,
                break_result: self.params.break_result,
                continue_id: self.params.continue_id,
                exit_id: self.params.exit_id,
            },
        });

        self.switch(next);

        Ok(())
    }

    fn lower_closure(&mut self, func: &'a Function, span: Span) -> Result<()> {
        let unpack = self.lower_pattern_sig(&func.params)?;
        let sig = self.unpacktab.id(&unpack);
        let fid = self
            .graph
            .alloc_func(sig, None, &func.body.vars, Some(self.block.scope));
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
        );
        let bodyid = self.graph.alloc_block(self.block.func, bscope);
        let span = node.while_span;
        self.block.term = Term(TermInfo::Branch(test), span);
        self.link(test);
        self.switch(test);
        let (bind, bind_params) =
            self.lower_cond(&node.expr, node.bind.as_ref(), bscope, bodyid, next, span)?;
        self.queue(Work {
            bb: bodyid,
            ast: WorkAst::Block(&node.body, false),
            params: Params {
                bind,
                bind_params,
                mode: self.params.mode.clone(),
                is_top_level: false,
                next_id: Some(test),
                break_id: Some(next),
                break_result: false,
                continue_id: Some(test),
                exit_id: self.params.exit_id,
            },
        });
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
        );
        let bodyid = self.graph.alloc_block(self.block.func, bscope);
        let binds = self.pattern_plan(self.graph.scope(bscope), &node.bind)?;
        let bind_params = Some(Defaults::Pattern(&node.bind));
        // FIXME: include span of keyword, not of block
        self.queue(Work {
            bb: bodyid,
            ast: WorkAst::Block(&node.body, false),
            params: Params {
                bind: Some(binds),
                bind_params,
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

    fn lower_pattern_sig(&mut self, items: &'a [PatItem]) -> Result<sig::Unpack> {
        let mut required = 0;
        let mut optional = Vec::new();
        let mut keys = Vec::new();
        let mut variadic = dolang_bytecode::Variadic::NONE;

        for item in items.iter() {
            match item {
                PatItem::Pos { default: None, .. } => required += 1,
                PatItem::Pos {
                    default: Some(default),
                    ..
                } => optional.push(self.lower_default_const(default)),
                PatItem::Key {
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
                PatItem::ConstKey {
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
                PatItem::Rest { kind, ident, .. } => {
                    use dolang_bytecode::{Rest, Variadic};
                    let rest = if ident.is_some() {
                        Rest::Capture
                    } else {
                        Rest::Discard
                    };
                    // The parser allows only `...` alone, or `*` followed by `**`
                    variadic = match (kind, variadic) {
                        (RestKind::Mixed, Variadic::NONE) => match rest {
                            Rest::Capture => Variadic::Capture,
                            _ => Variadic::Discard,
                        },
                        (RestKind::Pos, Variadic::NONE) => Variadic::Split(rest, Rest::None),
                        (RestKind::Key, Variadic::Split(pos, Rest::None)) => {
                            Variadic::Split(pos, rest)
                        }
                        _ => unreachable!("invalid rest items"),
                    };
                }
            }
        }

        Ok(sig::Unpack::new(required, optional, keys, variadic))
    }

    fn lower_non_const_defaults(&mut self, items: &'a [PatItem], span: Span) -> Result<()> {
        for item in items {
            let (default, bind) = match item {
                PatItem::Pos { default, bind, .. }
                | PatItem::Key { default, bind, .. }
                | PatItem::ConstKey { default, bind, .. } => (default, bind),
                PatItem::Rest { .. } => continue,
            };
            let ident = match bind {
                PatBind::Ident(ident) => ident,
                // A sub-pattern has no default of its own, but its items may
                PatBind::Nested { pattern, .. } => {
                    self.lower_pattern_defaults(pattern, span)?;
                    continue;
                }
            };
            let Some(default) = default.as_ref().filter(|d| d.fold.is_none()) else {
                continue;
            };

            let res = ident.res.as_ref().expect("unresolved item");
            let var = self.resolve_var(res.index, res.depth);

            // Load the current value of this item
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

    fn lower_default_const(&mut self, default: &PatDefault) -> constant::Id {
        match &default.fold {
            Some(fold) => self.lower_const(fold),
            None => self.sentinel_const(),
        }
    }

    fn sentinel_const(&mut self) -> constant::Id {
        *self.sentinel_const.get_or_init(|| {
            let sym_id = self.symtab.fresh(self.bintab.id_str("default"));
            self.consttab.sym(sym_id)
        })
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
        let unpack = self.lower_pattern_sig(&node.func.params)?;
        let name = node.ident.span;
        let res = &node.ident.res;
        let sig = self.unpacktab.id(&unpack);
        let fid = self.graph.alloc_func(
            sig,
            Some(name),
            &node.func.body.vars,
            Some(self.block.scope),
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

    fn lower_class_method_value(&mut self, node: &'a Method, class_name: Span) -> Result<()> {
        self.lower_decorator_exprs(&node.decorators)?;
        let unpack = self.lower_pattern_sig(&node.func.params)?;
        let name = if node.special.is_some() {
            node.name_span.before_left_char() | node.name_span.after_right_char()
        } else {
            node.name_span
        };
        let sig = self.unpacktab.id(&unpack);
        let fid = self.graph.alloc_func(
            sig,
            Some(name),
            &node.func.body.vars,
            Some(self.block.scope),
        );
        self.graph.func_mut(fid).class_name = Some(class_name);
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
        Ok(())
    }

    fn lower_member_sym_value(&mut self, sym: sym::Id, span: Span) {
        let sym = self.consttab.sym(sym);
        self.block.insts.push(Inst(InstInfo::LoadConst(sym), span));
    }

    fn lower_field_init_value(&mut self, field: &'a crate::ast::FieldDecl) -> Result<()> {
        let span = field
            .fields
            .first()
            .map(|field| field.ident.span)
            .unwrap_or(field.field_span);
        match &field.init {
            FieldInit::None => {
                self.lower_load_nil(field.field_span);
            }
            FieldInit::Const(_, value) => {
                let cid = self.lower_const(value);
                self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
            }
            FieldInit::Expr(expr) => {
                self.lower_expr(expr)?;
            }
            FieldInit::Thunk(func) => {
                self.lower_closure(func, span)?;
            }
        }
        Ok(())
    }

    fn lower_class_super(&mut self, node: &'a ClassSuper) {
        let res = node.ident.res.as_ref().expect("unresolved superclass root");
        self.lower_load(res, node.ident.span);
        for field in &node.fields {
            let sym = self.symtab.id(&self.bintab.id_str(self.file.str(*field)));
            self.block
                .insts
                .push(Inst(InstInfo::Get(sym), field.before_left_char()));
        }
    }

    fn lower_field_name_sym(&mut self, field: &'a crate::ast::FieldName) -> sym::Id {
        field.private_sym.unwrap_or_else(|| {
            self.symtab
                .id(&self.bintab.id_str(self.file.str(field.ident.span)))
        })
    }

    fn lower_method_sym(&mut self, def: &'a Method) -> sym::Id {
        def.private_sym.unwrap_or_else(|| {
            if let Some(method) = def.special {
                self.symtab.id(&self.bintab.id_str(method.sym()))
            } else {
                self.symtab
                    .id(&self.bintab.id_str(self.file.str(def.name_span)))
            }
        })
    }

    fn lower_class(&mut self, node: &'a Class, want_result: bool) -> Result<()> {
        let span = node.class_span;

        self.lower_decorator_exprs(&node.decorators)?;

        let name = self.file.str(node.ident.span).to_owned();
        let class_name = self.consttab.str(self.bintab.id_str(&name));
        self.block
            .insts
            .push(Inst(InstInfo::LoadConst(class_name), span));

        let module_name = match self.params.mode {
            Mode::Module { name } => name,
            _ => "",
        };
        let module_name = self.consttab.str(self.bintab.id_str(module_name));
        self.block
            .insts
            .push(Inst(InstInfo::LoadConst(module_name), span));

        let super_sym = self.symtab.id(&self.bintab.id_str("super"));
        let field_sym = self.symtab.id(&self.bintab.id_str("field"));
        let field_thunk_sym = self.symtab.id(&self.bintab.id_str("field_thunk"));
        let class_field_sym = self.symtab.id(&self.bintab.id_str("class_field"));
        let class_field_thunk_sym = self.symtab.id(&self.bintab.id_str("class_field_thunk"));
        let static_field_sym = self.symtab.id(&self.bintab.id_str("static_field"));
        let method_sym = self.symtab.id(&self.bintab.id_str("method"));

        // Type-only supertypes and methods exist only for documentation
        let supers = node.super_refs.iter().filter(|s| !s.type_only);
        for super_ref in supers.clone() {
            self.lower_class_super(super_ref);
        }

        for member in &node.body.members {
            match member {
                ClassMember::Field(field) => {
                    let Some((first, rest)) = field.fields.split_first() else {
                        continue;
                    };
                    let sym = self.lower_field_name_sym(first);
                    self.lower_member_sym_value(sym, first.ident.span);
                    self.lower_field_init_value(field)?;
                    match &field.init {
                        FieldInit::None => {
                            for name in rest {
                                let sym = self.lower_field_name_sym(name);
                                self.lower_member_sym_value(sym, name.ident.span);
                                self.lower_load_nil(name.ident.span);
                            }
                        }
                        FieldInit::Const(_, value) => {
                            let cid = self.lower_const(value);
                            for name in rest {
                                let sym = self.lower_field_name_sym(name);
                                self.lower_member_sym_value(sym, name.ident.span);
                                self.block
                                    .insts
                                    .push(Inst(InstInfo::LoadConst(cid), name.ident.span));
                            }
                        }
                        _ => {
                            for name in rest {
                                self.block.insts.push(Inst(InstInfo::Dup, name.ident.span));
                                let sym = self.lower_field_name_sym(name);
                                self.lower_member_sym_value(sym, name.ident.span);
                                self.block
                                    .insts
                                    .push(Inst(InstInfo::Swap(0, 1), name.ident.span));
                            }
                        }
                    }
                }
                ClassMember::Method(def) if def.type_only => {}
                ClassMember::Method(def) => {
                    let sym = self.lower_method_sym(def);
                    self.lower_member_sym_value(sym, member.span());
                    self.lower_class_method_value(def, node.ident.span)?;
                }
            }
        }

        let mut class_sig_args =
            Vec::with_capacity(2 + node.super_refs.len() + node.body.members.len() * 2);
        class_sig_args.push(sig::Arg::Value);
        class_sig_args.push(sig::Arg::Value);
        class_sig_args.extend(std::iter::repeat_n(
            sig::Arg::Key(super_sym),
            supers.count(),
        ));
        for member in &node.body.members {
            match member {
                ClassMember::Field(field) => {
                    // A static field is evaluated once, at class creation, so its
                    // initializer is lowered inline rather than as a thunk. Class
                    // and instance fields keep the thunk: it is re-run per subclass
                    // and per instance respectively, which is what gives each its
                    // own storage.
                    let is_thunk = matches!(&field.init, FieldInit::Thunk(_));
                    let key_sym = match (field.scope, is_thunk) {
                        (MemberScope::Instance, true) => field_thunk_sym,
                        (MemberScope::Instance, false) => field_sym,
                        (MemberScope::Class, true) => class_field_thunk_sym,
                        (MemberScope::Class, false) => class_field_sym,
                        (MemberScope::Static, _) => static_field_sym,
                    };
                    for _ in &field.fields {
                        class_sig_args.push(sig::Arg::Key(key_sym));
                        class_sig_args.push(sig::Arg::Value);
                    }
                }
                ClassMember::Method(def) if def.type_only => {}
                ClassMember::Method(_) => {
                    class_sig_args.push(sig::Arg::Key(method_sym));
                    class_sig_args.push(sig::Arg::Value);
                }
            }
        }
        let class_sig = sig::Pack::new(class_sig_args.into_iter());
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

    fn lower_load_nil(&mut self, span: Span) {
        self.block
            .insts
            .push(Inst(InstInfo::LoadConst(self.consttab.nil()), span));
    }

    fn lower_import(&mut self, import: &Import, want_result: bool) -> Result<()> {
        let span = import.span();
        for import in &import.elements {
            let module = match import {
                ImportElement::ModuleAsIs { module, .. }
                | ImportElement::ModuleRenamed { module, .. } => *module,
                ImportElement::Items { module, .. } => *module,
            };

            match import {
                ImportElement::ModuleAsIs {
                    bind,
                    insert,
                    type_only,
                    ..
                } => {
                    if type_only.is_some() {
                        continue;
                    }
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
                ImportElement::ModuleRenamed {
                    bind, type_only, ..
                } => {
                    if type_only.is_some() {
                        continue;
                    }
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
                    // An item named only in types is not imported, nor is a module
                    // whose items all are
                    let count = items.iter().filter(|item| !item.is_type_only()).count();
                    if count == 0 {
                        continue;
                    }
                    let cid = self.consttab.str(self.bintab.id_str(self.file.str(module)));
                    self.block.insts.push(Inst(InstInfo::LoadConst(cid), span));
                    let get = self.symtab.id(&self.bintab.id_str("get"));
                    let call = sig::Pack::new([sig::Arg::Key(get)].into_iter());
                    let sig = self.packtab.id(&call);
                    self.block
                        .insts
                        .push(Inst(InstInfo::Builtin(builtin::IMPORT, sig), span));

                    for (i, item) in items.iter().filter(|item| !item.is_type_only()).enumerate() {
                        let (item_span, bind) = match item {
                            ImportItem::Renamed { item, bind, .. } => (*item, bind),
                            ImportItem::AsIs { bind, .. } => (bind.span, bind),
                        };
                        if i + 1 != count {
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

    fn lower_block(
        &mut self,
        block: &'a Block,
        mut want_result: bool,
        stub_span: Option<Span>,
        guard: Option<(&'a Guard, cfg::BlockId)>,
    ) -> Result<()> {
        let scope = self.graph.scope(self.block.scope);
        // An empty stub block has no span of its own, so use its marker for the
        // parameter-binding prologue as well as the builtin call.
        let span = stub_span.unwrap_or_else(|| block.span());

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
                        if fields.iter().all(|f| f.unused) {
                            // Unused, skip entirely
                            continue;
                        }
                        module
                    }
                    PreludeImport::ModuleAsIs { module, unused, .. }
                    | PreludeImport::ModuleRenamed { module, unused, .. } => {
                        if *unused {
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
                        let used: Vec<_> = fields.iter().filter(|f| !f.unused).collect();
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

        self.lower_prologue_bind(span)?;
        if let Some((guard, fail)) = guard {
            self.lower_guard(guard, fail, span)?;
        }

        // End prologue
        if let Some(span) = stub_span {
            let sig = self.packtab.id(&sig::Pack::new(std::iter::empty()));
            self.block
                .insts
                .push(Inst(InstInfo::Builtin(builtin::STUB, sig), span));
            self.block.term = Term(TermInfo::Branch(self.params.exit_id), span);
            self.link(self.params.exit_id);
            return Ok(());
        }
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
        let empty = sig::Unpack::new(0, [], [], dolang_bytecode::Variadic::NONE);
        let sig = self.unpacktab.id(&empty);
        let fid = self.graph.alloc_nl_guard(sig, Some(self.block.scope));
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
            PrimStmt::Match(node) => {
                self.lower_match(node, want_result)?;
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
            Stmt::Class(class) if class.is_protocol() => {
                if want_result {
                    self.lower_load_nil(class.span());
                }
                Ok(false)
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
            Stmt::Def(node) if node.is_type_only() => {
                if want_result {
                    self.lower_load_nil(node.span());
                }
                Ok(false)
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
            Stmt::TypeAlias(node) => {
                if want_result {
                    self.lower_load_nil(node.span());
                }
                Ok(false)
            }
        }
    }

    fn lower_function(&mut self, function: &'a Function, sig: sig::UnpackId) -> Result<()> {
        // Compute order in which to move arguments into locals or upvars
        self.params.bind =
            Some(self.bind_plan(self.graph.scope(self.block.scope), &function.params, sig)?);
        self.params.bind_params = Some(Defaults::Items(&function.params));
        self.lower_block(&function.body, true, function.stub_span, None)?;
        Ok(())
    }

    /// The values an unpack with `sig` leaves on the operand stack for `items`,
    /// from the top of the stack.
    fn unpack_order_in_scope(
        &mut self,
        scope: cfg::ScopeRef<'a>,
        items: &'a [PatItem],
        sig: sig::UnpackId,
    ) -> Vec<Slot<'a>> {
        let pos: Vec<_> = items
            .iter()
            .filter_map(|p| {
                if let PatItem::Pos { bind, .. } = p {
                    Some(bind)
                } else {
                    None
                }
            })
            .collect();
        let mut sym_keys: Vec<_> = items
            .iter()
            .filter_map(|p| {
                if let PatItem::Key { key_span, bind, .. } = p {
                    Some((
                        self.symtab
                            .id(&self.bintab.id_str(self.file.str(*key_span))),
                        bind,
                    ))
                } else {
                    None
                }
            })
            .collect();
        let mut const_keys: Vec<_> = items
            .iter()
            .filter_map(|p| {
                if let PatItem::ConstKey {
                    key_const, bind, ..
                } = p
                {
                    Some((self.lower_const(key_const), bind))
                } else {
                    None
                }
            })
            .collect();
        // Capturing rests, which take the last slots in item order
        let rests: Vec<_> = items
            .iter()
            .filter_map(|p| {
                if let PatItem::Rest { ident, .. } = p {
                    ident.as_ref()
                } else {
                    None
                }
            })
            .collect();
        sym_keys.sort_by_key(|(sym, _)| *sym);
        const_keys.sort_by_key(|(c, _)| *c);
        let var = |this: &mut Self, ident: &Ident| {
            let res = ident.res.as_ref().expect("unresolved item");
            Slot::Var(this.resolve_var_in_scope(cfg::ScopeRef::clone(&scope), res.index, res.depth))
        };
        let slot = |this: &mut Self, bind: &'a PatBind| match bind {
            PatBind::Ident(ident) => var(this, ident),
            PatBind::Nested { pattern, .. } => Slot::Pattern(pattern),
        };
        let unpack = &self.unpacktab[sig];
        let keys: Vec<_> = unpack
            .iter_keys()
            .rev()
            .map(|key| key.kind.clone())
            .collect();
        let mut slots = Vec::new();
        for id in rests.iter().rev() {
            slots.push(var(self, id));
        }
        for kind in keys {
            let bind = match kind {
                sig::UnpackKeyKind::Sym(sym) => {
                    let index = sym_keys
                        .binary_search_by_key(&sym, |(s, _)| *s)
                        .expect("key symbol not in pattern items?!");
                    sym_keys[index].1
                }
                sig::UnpackKeyKind::Const(c) => {
                    let index = const_keys
                        .binary_search_by_key(&c, |(const_id, _)| *const_id)
                        .expect("constant key not in pattern items?!");
                    const_keys[index].1
                }
            };
            slots.push(slot(self, bind));
        }
        for bind in pos.iter().rev() {
            slots.push(slot(self, bind));
        }
        slots
    }

    /// Plan the binding of the values an unpack with `sig` leaves on the operand
    /// stack for `items`, unpacking sub-patterns in turn.
    fn bind_plan(
        &mut self,
        scope: cfg::ScopeRef<'a>,
        items: &'a [PatItem],
        sig: sig::UnpackId,
    ) -> Result<BindPlan> {
        let stack = self.unpack_order_in_scope(cfg::ScopeRef::clone(&scope), items, sig);
        self.plan_slots(scope, stack)
    }

    /// Evaluate the non-constant defaults of a bound pattern. An alternation's
    /// are those of the alternative that matched.
    fn lower_pattern_defaults(&mut self, pattern: &'a Pattern, span: Span) -> Result<()> {
        match pattern {
            Pattern::Constant { .. } | Pattern::Ident(_) => Ok(()),
            Pattern::Unpack(items) => self.lower_non_const_defaults(items, span),
            Pattern::TypeTest(test) => self.lower_pattern_defaults(&test.pattern, span),
            Pattern::Alt(alt) => {
                let Some(index) = alt.indicator else {
                    return Ok(());
                };
                let indicator = Res {
                    index,
                    depth: 0,
                    node: None,
                };
                let join = self.graph.alloc_block(self.block.func, self.block.scope);
                let count = alt.alts.len();
                for (i, pattern) in alt.alts.iter().enumerate() {
                    if i + 1 < count {
                        let case = self.graph.alloc_block(self.block.func, self.block.scope);
                        let next = self.graph.alloc_block(self.block.func, self.block.scope);
                        self.lower_load(&indicator, span);
                        let i = self.consttab.int(i as constant::Int);
                        self.block.insts.push(Inst(InstInfo::LoadConst(i), span));
                        self.block.insts.push(Inst(InstInfo::Eq, span));
                        self.block.term = Term(TermInfo::If(case, next), span);
                        self.link(case);
                        self.link(next);
                        self.switch(case);
                        self.lower_pattern_defaults(pattern, span)?;
                        self.block.term = Term(TermInfo::Branch(join), span);
                        self.link(join);
                        self.switch(next);
                    } else {
                        self.lower_pattern_defaults(pattern, span)?;
                        self.block.term = Term(TermInfo::Branch(join), span);
                        self.link(join);
                    }
                }
                self.switch(join);
                Ok(())
            }
        }
    }

    fn pattern_plan(&mut self, scope: cfg::ScopeRef<'a>, pattern: &'a Pattern) -> Result<BindPlan> {
        self.plan_slots(scope, vec![Slot::Pattern(pattern)])
    }

    fn plan_slots(
        &mut self,
        scope: cfg::ScopeRef<'a>,
        mut stack: Vec<Slot<'a>>,
    ) -> Result<BindPlan> {
        let mut steps = Vec::new();
        while let Some(depth) = stack
            .iter()
            .position(|slot| matches!(slot, Slot::Pattern(_)))
        {
            let Slot::Pattern(pattern) = stack[depth] else {
                unreachable!()
            };
            // Tests leave the value in place, so a failure discards the whole stack.
            let others = stack.len();
            let op = match pattern {
                // A plain binding needs no stack operation.
                Pattern::Ident(PatIdent { ident, .. }) => {
                    let res = ident.res.as_ref().expect("unresolved pattern binding");
                    stack[depth] = Slot::Var(self.resolve_var_in_scope(
                        cfg::ScopeRef::clone(&scope),
                        res.index,
                        res.depth,
                    ));
                    continue;
                }
                Pattern::Unpack(items) => {
                    stack.swap(0, depth);
                    stack.remove(0);
                    let unpack = self.lower_pattern_sig(items)?;
                    let sig = self.unpacktab.id(&unpack);
                    let slots =
                        self.unpack_order_in_scope(cfg::ScopeRef::clone(&scope), items, sig);
                    stack.splice(0..0, slots);
                    steps.push(BindStep {
                        depth,
                        op: BindOp::Unpack(sig),
                        others: others - 1,
                    });
                    continue;
                }
                Pattern::Constant { value, .. } => {
                    stack[depth] = Slot::Discard;
                    BindOp::Constant(self.lower_const(value))
                }
                Pattern::TypeTest(test) => {
                    let res = test
                        .class
                        .ident
                        .res
                        .as_ref()
                        .expect("unresolved pattern class");
                    let var = self.resolve_var_in_scope(
                        cfg::ScopeRef::clone(&scope),
                        res.index,
                        res.depth,
                    );
                    let fields = test
                        .class
                        .fields
                        .iter()
                        .map(|field| self.symtab.id(&self.bintab.id_str(self.file.str(*field))))
                        .collect();
                    stack[depth] = Slot::Pattern(&test.pattern);
                    BindOp::TypeTest { var, fields }
                }
                Pattern::Alt(alt) => {
                    stack.swap(0, depth);
                    stack.remove(0);
                    let mut alts = Vec::new();
                    let mut canon = Vec::new();
                    for pattern in &alt.alts {
                        let plan = self.plan_slots(
                            cfg::ScopeRef::clone(&scope),
                            vec![Slot::Pattern(pattern)],
                        )?;
                        for var in plan.vars.iter().flatten() {
                            if !canon.contains(var) {
                                canon.push(*var);
                            }
                        }
                        alts.push(plan);
                    }
                    let indicator = alt.indicator.map(|index| {
                        self.resolve_var_in_scope(cfg::ScopeRef::clone(&scope), index, 0)
                    });
                    let slots = indicator.iter().chain(&canon).map(|var| Slot::Var(*var));
                    stack.splice(0..0, slots);
                    steps.push(BindStep {
                        depth,
                        op: BindOp::Alt {
                            alts,
                            canon,
                            indicator,
                        },
                        others: others - 1,
                    });
                    continue;
                }
            };
            steps.push(BindStep { depth, op, others });
        }
        let vars = stack
            .into_iter()
            .map(|slot| match slot {
                Slot::Var(var) => Some(var),
                Slot::Discard => None,
                Slot::Pattern(_) => unreachable!(),
            })
            .collect();
        Ok(BindPlan { steps, vars })
    }

    /// Run a plan's tests and unpacks, which raise on a mismatch, and store the
    /// values left.
    fn lower_bind_plan(&mut self, plan: BindPlan, span: Span) {
        self.lower_steps(plan.steps, Fail::Raise, span);
        for var in plan.vars {
            if let Some(var) = var {
                self.lower_store(span, var);
            } else {
                self.block.insts.push(Inst(InstInfo::Pop, span));
            }
        }
    }

    /// Run a plan's tests and unpacks, continuing at `fail` on a mismatch, and on
    /// a match in the current block.
    fn lower_steps(&mut self, steps: Vec<BindStep>, fail: Fail, span: Span) {
        let mut discards = Vec::new();
        for step in steps {
            let next = match (step.op, fail) {
                (
                    BindOp::Alt {
                        alts,
                        canon,
                        indicator,
                    },
                    fail,
                ) => {
                    self.lower_swap(step.depth, span);
                    // The last alternative takes the value, leaving the others
                    let fail = match fail {
                        Fail::Raise => Fail::Raise,
                        Fail::Goto { target, below } => Fail::Goto {
                            target,
                            below: below + step.others,
                        },
                    };
                    let join = self.graph.alloc_block(self.block.func, self.block.scope);
                    self.lower_alts(alts, &canon, indicator, fail, join, span);
                    join
                }
                (BindOp::Unpack(sig), Fail::Raise) => {
                    self.lower_swap(step.depth, span);
                    self.block.insts.push(Inst(InstInfo::Unpack(sig), span));
                    continue;
                }
                (op, Fail::Raise) => {
                    self.lower_test(op, step.depth, true, span);
                    continue;
                }
                (op, Fail::Goto { target, below }) => {
                    let mismatch = self.discard_block(
                        Self::discards_to(&mut discards, target),
                        step.others + below,
                        target,
                        span,
                    );
                    let next = self.graph.alloc_block(self.block.func, self.block.scope);
                    match op {
                        BindOp::Unpack(sig) => {
                            self.lower_swap(step.depth, span);
                            self.block.term = Term(TermInfo::UnpackIf(sig, next, mismatch), span)
                        }
                        op => {
                            self.lower_test(op, step.depth, false, span);
                            self.block.term = Term(TermInfo::If(next, mismatch), span);
                        }
                    }
                    self.link(next);
                    self.link(mismatch);
                    next
                }
            };
            self.switch(next);
        }
    }

    /// Bring the value at `depth` to the top of the stack
    fn lower_swap(&mut self, depth: usize, span: Span) {
        if depth > 0 {
            self.block.insts.push(Inst(InstInfo::Swap(0, depth), span));
        }
    }

    /// The discard chain for failures continuing at `target`
    fn discards_to(
        discards: &mut Vec<(cfg::BlockId, Vec<cfg::BlockId>)>,
        target: cfg::BlockId,
    ) -> &mut Vec<cfg::BlockId> {
        let index = match discards.iter().position(|(t, _)| *t == target) {
            Some(index) => index,
            None => {
                discards.push((target, Vec::new()));
                discards.len() - 1
            }
        };
        &mut discards[index].1
    }

    /// Try alternatives in turn against the value on top of the stack, the last
    /// continuing at `fail` on a mismatch. A match leaves `indicator`'s value
    /// above `canon`'s in place of the value, and continues at `join`.
    fn lower_alts(
        &mut self,
        alts: Vec<BindPlan>,
        canon: &[Var],
        indicator: Option<Var>,
        fail: Fail,
        join: cfg::BlockId,
        span: Span,
    ) {
        let count = alts.len();
        for (index, plan) in alts.into_iter().enumerate() {
            // Each alternative but the last matches a copy, keeping the value
            // for the next
            let next = (index + 1 < count)
                .then(|| self.graph.alloc_block(self.block.func, self.block.scope));
            let fail = match next {
                Some(target) => {
                    self.block.insts.push(Inst(InstInfo::Dup, span));
                    Fail::Goto { target, below: 0 }
                }
                None => fail,
            };
            self.lower_steps(plan.steps, fail, span);
            self.lower_canon(plan.vars, next.is_some(), canon, span);
            if indicator.is_some() {
                let value = self.consttab.int(index as constant::Int);
                self.block
                    .insts
                    .push(Inst(InstInfo::LoadConst(value), span));
            }
            self.block.term = Term(TermInfo::Branch(join), span);
            self.link(join);
            if let Some(next) = next {
                self.switch(next);
            }
        }
    }

    /// Rearrange the values an alternative left, from the top, and the matched
    /// value below them if `original`, into the values of `canon`, from the top.
    fn lower_canon(&mut self, vars: Vec<Option<Var>>, original: bool, canon: &[Var], span: Span) {
        let mut stack = vars;
        if original {
            stack.push(None);
        }
        while let Some(depth) = stack.iter().position(Option::is_none) {
            if depth > 0 {
                self.block.insts.push(Inst(InstInfo::Swap(0, depth), span));
                stack.swap(0, depth);
            }
            self.block.insts.push(Inst(InstInfo::Pop, span));
            stack.remove(0);
        }
        // Another alternative's variables are left nil
        for var in canon {
            if !stack.contains(&Some(*var)) {
                let nil = self.consttab.nil();
                self.block.insts.push(Inst(InstInfo::LoadConst(nil), span));
                stack.insert(0, Some(*var));
            }
        }
        for (depth, var) in canon.iter().enumerate() {
            let at =
                (stack.iter().position(|slot| *slot == Some(*var))).expect("a canonical variable");
            if at != depth {
                self.block.insts.push(Inst(InstInfo::Swap(depth, at), span));
                stack.swap(depth, at);
            }
        }
    }

    /// Test a copy of the value at `depth`, leaving a boolean unless `assert`,
    /// which raises on a mismatch instead.
    fn lower_test(&mut self, op: BindOp, depth: usize, assert: bool, span: Span) {
        let pick = if depth == 0 {
            InstInfo::Dup
        } else {
            InstInfo::Pick(depth)
        };
        self.block.insts.push(Inst(pick, span));
        let builtin = match op {
            BindOp::Unpack(_) | BindOp::Alt { .. } => unreachable!(),
            BindOp::Constant(id) => {
                self.block.insts.push(Inst(InstInfo::LoadConst(id), span));
                if !assert {
                    self.block.insts.push(Inst(InstInfo::Eq, span));
                    return;
                }
                builtin::VALUE_ASSERT
            }
            BindOp::TypeTest { var, fields } => {
                let load = match var {
                    Var::Local(index) => InstInfo::LoadLocal(index),
                    Var::Upvar(index, depth) => InstInfo::LoadUpvar(index, depth),
                };
                self.block.insts.push(Inst(load, span));
                for field in fields {
                    self.block.insts.push(Inst(InstInfo::Get(field), span));
                }
                if assert {
                    builtin::TYPE_ASSERT
                } else {
                    builtin::TYPE_TEST
                }
            }
        };
        let args = self.packtab.id(&sig::Pack::new(
            [sig::Arg::Value, sig::Arg::Value].into_iter(),
        ));
        self.block
            .insts
            .push(Inst(InstInfo::Builtin(builtin, args), span));
        if assert {
            self.block.insts.push(Inst(InstInfo::Pop, span));
        }
    }

    /// The block that discards `count` values from the operand stack, then
    /// continues at `target`.
    ///
    /// `blocks` caches the chain built so far: its `n`th block discards `n + 1`
    /// values by discarding one and continuing at the block before it.
    fn discard_block(
        &mut self,
        blocks: &mut Vec<cfg::BlockId>,
        count: usize,
        target: cfg::BlockId,
        span: Span,
    ) -> cfg::BlockId {
        let back = self.bb;
        while blocks.len() < count {
            let next = blocks.last().copied().unwrap_or(target);
            let id = self.graph.alloc_block(self.block.func, self.block.scope);
            self.switch(id);
            self.block.insts.push(Inst(InstInfo::Pop, span));
            self.block.term = Term(TermInfo::Branch(next), span);
            self.link(next);
            blocks.push(id);
        }
        self.switch(back);
        count.checked_sub(1).map_or(target, |index| blocks[index])
    }

    /// Bind the values the body's caller left on the operand stack, as the body's
    /// prologue.
    fn lower_prologue_bind(&mut self, span: Span) -> Result<()> {
        if let Some(plan) = self.params.bind.take() {
            self.lower_bind_plan(plan, span);
        }
        match self.params.bind_params {
            Some(Defaults::Items(params)) => self.lower_non_const_defaults(params, span)?,
            Some(Defaults::Pattern(pattern)) => self.lower_pattern_defaults(pattern, span)?,
            None => {}
        }
        Ok(())
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

        self.lower_prologue_bind(span)?;

        let mut sig = Vec::new();
        self.block.insts.push(Inst(InstInfo::Dup, span));
        // End prologue
        for arg in body.iter() {
            sig.push(self.lower_arg(arg)?);
        }
        let sig = sig::Pack::new(sig.into_iter());
        let sig = self.packtab.id(&sig);

        self.block.insts.push(Inst(
            InstInfo::MethodCall(self.symtab.id(&self.bintab.id_str("push")), sig),
            span,
        ));
        self.block.insts.push(Inst(InstInfo::Pop, span));

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

        self.lower_prologue_bind(span)?;

        let mut sig = Vec::new();
        self.block.insts.push(Inst(InstInfo::Dup, span));
        // End prologue
        for elem in body.iter() {
            sig.push(self.lower_array_elem(elem)?);
        }
        let sig = sig::Pack::new(sig.into_iter());
        let sig = self.packtab.id(&sig);

        self.block.insts.push(Inst(
            InstInfo::MethodCall(self.symtab.id(&self.bintab.id_str("push")), sig),
            span,
        ));
        self.block.insts.push(Inst(InstInfo::Pop, span));

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

        self.lower_prologue_bind(span)?;

        let mut sig = Vec::new();
        self.block.insts.push(Inst(InstInfo::Dup, span));
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

        self.block.insts.push(Inst(
            InstInfo::MethodCall(self.symtab.id(&self.bintab.id_str("push")), sig),
            span,
        ));
        self.block.insts.push(Inst(InstInfo::Pop, span));

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

    pub(crate) fn run(&mut self, root: &Root) -> Result<cfg::Graph> {
        let mut graph = cfg::Graph::new();
        let empty = sig::Unpack::new(0, [], [], dolang_bytecode::Variadic::NONE);
        let sig = self.unpacktab.id(&empty);
        let fid = graph.alloc_func(sig, None, &root.0.body.vars, None);
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
                sentinel_const: &self.sentinel_const,
                graph: &graph,
                bb: work.bb,
                block: graph.block_mut(work.bb),
                params: work.params,
                queue: &mut queue,
            };
            match work.ast {
                WorkAst::Function(function, sig) => scope.lower_function(function, sig)?,
                WorkAst::Block(block, want_result) => {
                    scope.lower_block(block, want_result, None, None)?
                }
                WorkAst::Arm(arm, want_result, fail) => scope.lower_block(
                    &arm.body,
                    want_result,
                    None,
                    arm.guard.as_ref().map(|guard| (guard, fail)),
                )?,
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
