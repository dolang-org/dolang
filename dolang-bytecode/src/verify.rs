use std::{
    collections::HashSet,
    fmt::{self, Debug, Display, Formatter},
    io, result,
};

use super::{
    Arg, BUILTINS, BlockState, Certificate, DecError, Func, Inst, InstDecoder, InstOffsets,
    LocalState, Phase, limit,
};

#[derive(PartialEq, Eq)]
pub(crate) enum InstError {
    InvalidOpcode,
    Truncated,
    IntegerOverflow,
    OperandDepthConflict {
        from: usize,
        into: usize,
    },
    UpvarDepthConflict {
        from: usize,
        into: usize,
    },
    UpvarCountConflict {
        depth: usize,
        from: usize,
        into: usize,
    },
    NoOpSwap,
    OperandIndexOutOfBounds(usize),
    OperandUnderflow,
    BranchTargetOutOfBounds,
    BranchTargetInvalid,
    ConstIndexOutOfBounds,
    SymIndexOutOfBounds,
    BuiltinIndexOutOfBounds,
    LocalIndexOutOfBounds,
    LocalLoadUninitialized,
    UpvarDepthOutOfBounds,
    UpvarIndexOutOfBounds,
    UpvarPushExceedsLimit,
    UpvarUnderflow,
    PackIndexOutOfBounds,
    UnpackIndexOutOfBounds,
    FuncIndexOutOfBounds,
    UpvarSigMismatch,
    ReturnWithExcessOperands,
    ReturnWithExcessUpvars,
    InstructionUnreachable,
    NotAFixedPoint,
    ReifyMismatch(usize, usize),
}

impl Display for InstError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use InstError::*;

        match self {
            InvalidOpcode => write!(f, "invalid opcode"),
            Truncated => write!(f, "truncated instruction"),
            IntegerOverflow => write!(f, "integer overflow"),
            OperandDepthConflict { from, into } => {
                write!(f, "operand stack depth conflict ({from} != {into})")
            }
            UpvarDepthConflict { from, into } => {
                write!(f, "upvar stack depth conflict ({from} != {into})")
            }
            UpvarCountConflict { depth, from, into } => {
                write!(f, "upvar count conflict at depth {depth} ({from} != {into}")
            }
            NoOpSwap => write!(f, "swap immediates are the same"),
            OperandIndexOutOfBounds(i) => write!(f, "operand index out of bounds: {}", i),
            OperandUnderflow => write!(f, "operand stack underflow"),
            BranchTargetOutOfBounds => write!(f, "branch target out of bounds"),
            BranchTargetInvalid => write!(f, "branch target is not an instruction boundary"),
            ConstIndexOutOfBounds => write!(f, "constant table index out of bounds"),
            SymIndexOutOfBounds => write!(f, "symbol table index out of bounds"),
            BuiltinIndexOutOfBounds => write!(f, "builtin call index out of bounds"),
            LocalIndexOutOfBounds => write!(f, "local index out of bounds"),
            LocalLoadUninitialized => write!(f, "load of possibly uninitialized local"),
            UpvarDepthOutOfBounds => write!(f, "upvar depth out of bounds"),
            UpvarIndexOutOfBounds => write!(f, "upvar index out of bounds"),
            UpvarPushExceedsLimit => write!(f, "upvar limit exceeded"),
            UpvarUnderflow => write!(f, "upvar stack underflow"),
            PackIndexOutOfBounds => write!(f, "pack table index out of bounds"),
            UnpackIndexOutOfBounds => write!(f, "unpack table index out of bounds"),
            FuncIndexOutOfBounds => write!(f, "function table index out of bounds"),
            UpvarSigMismatch => write!(f, "closure upvar signature does not match state"),
            ReturnWithExcessOperands => write!(f, "return with excess operands on stack"),
            ReturnWithExcessUpvars => write!(f, "return with excess upvar records on stack"),
            InstructionUnreachable => write!(f, "syntactically unreachable instruction"),
            NotAFixedPoint => write!(f, "certificate is not a fixed point"),
            ReifyMismatch(i, j) => write!(f, "reify pack/upvar mismatch ({i} != {j})"),
        }
    }
}

impl Debug for InstError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(PartialEq, Eq)]
pub(crate) enum FuncError {
    Inst(usize, InstError, Option<String>),
    UnexpectedEnd,
    UpvarLimitExceeded,
    BasicBlockMismatch,
    OperandDepthMismatch,
    Unused,
}

impl FuncError {
    #[cfg(feature = "debug")]
    fn disass(bytecode: &[u8], offset: usize) -> Option<String> {
        let bytes = bytecode.get(offset..)?;
        let mut cursor = io::Cursor::new(bytes);
        let mut decoder = InstDecoder::new(&mut cursor);
        decoder.next()?.ok().map(|inst| inst.to_string())
    }

    #[cfg(not(feature = "debug"))]
    fn disass(_bytecode: &[u8], _offset: usize) -> Option<String> {
        None
    }

    fn inst(bytecode: &[u8], inst: usize, inner: InstError) -> Self {
        Self::Inst(inst, inner, Self::disass(bytecode, inst))
    }
}

impl Display for FuncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use FuncError::*;

        match self {
            Inst(inst, inner, None) => write!(f, "pc 0x{inst:x}: {inner}"),
            Inst(inst, inner, Some(disass)) => write!(f, "pc 0x{inst:x} [{disass}]: {inner}"),
            Unused => write!(f, "function is unused"),
            UnexpectedEnd => write!(f, "execution can fall off end of code"),
            UpvarLimitExceeded => write!(f, "upvar limit limit exceeded"),
            BasicBlockMismatch => write!(
                f,
                "certificate did not match expected basic block locations"
            ),
            OperandDepthMismatch => write!(
                f,
                "certificate did not match expected maximum operand stack depth"
            ),
        }
    }
}

impl Debug for FuncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

pub struct Error {
    func: usize,
    error: FuncError,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "function #{}: {}", self.func, self.error)
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

type FuncResult<T> = result::Result<T, FuncError>;

type FlowResult<T> = result::Result<T, InstError>;

pub type Result<T> = result::Result<T, Error>;

pub trait Context {
    type Phase: Phase;

    fn slice<'a>(&'a self, bytes: &'a <Self::Phase as Phase>::Bytes) -> &'a [u8];
    fn function(&self, index: usize) -> Option<&Func<Self::Phase>>;
    fn pack(&self, index: usize) -> Option<impl Iterator<Item = Arg>>;
    fn unpack_arity(&self, index: usize) -> Option<usize>;
    fn symbol_valid(&self, index: usize) -> bool;
    fn constant_valid(&self, index: usize) -> bool;
}

impl LocalState {
    fn merge(&self, into: &mut Self) -> FlowResult<()> {
        *into = match (self, &*into) {
            (LocalState::Value, LocalState::Value) => LocalState::Value,
            _ => LocalState::Invalid,
        };
        Ok(())
    }
}

impl Display for LocalState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(
            match self {
                LocalState::Invalid => "⊥",
                LocalState::Value => "·",
            },
            f,
        )
    }
}

impl BlockState {
    fn neutral() -> Self {
        Self {
            // Sentinel value indicating neutral element of merge operation
            operands: usize::MAX,
            locals: Default::default(),
            upvars: Default::default(),
        }
    }

    fn merge(&self, into: &mut Self) -> FlowResult<()> {
        if into.operands == usize::MAX {
            self.clone_into(into);
            return Ok(());
        }

        if self.operands != into.operands {
            return Err(InstError::OperandDepthConflict {
                from: self.operands,
                into: into.operands,
            });
        }
        if self.upvars.len() != into.upvars.len() {
            return Err(InstError::UpvarDepthConflict {
                from: self.upvars.len(),
                into: into.upvars.len(),
            });
        }
        for (i, (l, r)) in self.upvars.iter().zip(into.upvars.iter()).enumerate() {
            if l != r {
                return Err(InstError::UpvarCountConflict {
                    depth: self.upvars.len() - i - 1,
                    from: *l,
                    into: *r,
                });
            }
        }
        for (l, r) in self.locals.iter().zip(into.locals.iter_mut()) {
            l.merge(r)?
        }
        Ok(())
    }

    fn pop(&mut self) -> FlowResult<()> {
        self.operands = self
            .operands
            .checked_sub(1)
            .ok_or(InstError::OperandUnderflow)?;
        Ok(())
    }

    fn push(&mut self) {
        self.operands = self.operands.strict_add(1)
    }
}

impl Display for BlockState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "operands: {}", self.operands)?;
        write!(f, " locals: [")?;
        for local in self.locals.iter() {
            write!(f, "{local}")?
        }
        write!(f, "] upvars: [")?;
        for upvar in self.upvars.iter() {
            write!(f, "{upvar}")?
        }
        write!(f, "]")
    }
}

struct OffsetInfo {
    // Byte offset into bytecode
    offset: usize,
    // Should the instruction at this offset start a new basic block?
    start_bb: bool,
}

struct Block {
    // Byte offset into bytecode
    offset: usize,
    // Data flow state has changed (no input cert), or
    // Block was reached (input cert)
    mark: bool,
    // Data flow state
    state: BlockState,
}

// Tracks function use (closure construction) relationships
#[derive(Default, Clone)]
struct Used {
    used: bool,
    uses: HashSet<usize>,
}

struct FuncVerifier<'a, C: Context> {
    // Verification context
    ctx: &'a C,
    // Index of function being verified
    index: usize,
    // Function being verified
    func: &'a Func<C::Phase>,
    // Function use relations
    used: &'a mut [Used],
    // Byte offsets of logical instructions (and whether they start a basic block)
    offsets: Vec<OffsetInfo>,
    // Basic blocks
    blocks: Vec<Block>,
    // Queued block indices in data flow pass
    queue: Vec<usize>,
    // Maximum operand stack depth
    max_operand_depth: usize,
}

impl<'a, C: Context> FuncVerifier<'a, C> {
    fn new(ctx: &'a C, index: usize, func: &'a Func<C::Phase>, used: &'a mut [Used]) -> Self {
        Self {
            ctx,
            index,
            func,
            used,
            offsets: Vec::new(),
            blocks: Vec::new(),
            queue: Vec::new(),
            max_operand_depth: 0,
        }
    }

    // Iterate over instructions in bytecode slice from given offset
    fn insts<'b>(
        code: &'b [u8],
        start: usize,
    ) -> impl Iterator<Item = super::DecResult<InstOffsets>> + 'b {
        let mut cursor = io::Cursor::new(code);
        cursor.set_position(start as u64);
        InstDecoder::new(cursor).with_offsets()
    }

    // Iterate instructions that have been previously verified as well-formed
    fn insts_verified<'b>(code: &'b [u8], start: usize) -> impl Iterator<Item = InstOffsets> + 'b {
        Self::insts(code, start).map(|i| i.expect("I/O error on slice?!"))
    }

    // Pass 0: static limits, code well-formedness, branch target boundedness => offset array
    fn syntax(&mut self) -> FuncResult<()> {
        use Inst::*;
        let bytecode_len = self.ctx.slice(&self.func.bytecode).len();
        if self
            .func
            .upvars
            .iter()
            .fold(0usize, |acc, count| acc.saturating_add(*count))
            > limit::UPVAR_TOTAL
        {
            return Err(FuncError::UpvarLimitExceeded);
        }

        let mut need_bb = true;

        let bytecode = self.ctx.slice(&self.func.bytecode);

        let mut last = InstOffsets {
            before: 0usize,
            inst: Inst::Ret,
            after: 0usize,
        };

        for item in Self::insts(bytecode, 0) {
            last = item.map_err(|e| match e {
                DecError::InvalidOpcode(offset) => {
                    FuncError::inst(bytecode, offset, InstError::InvalidOpcode)
                }
                DecError::Truncated(offset) => {
                    FuncError::inst(bytecode, offset, InstError::Truncated)
                }
                DecError::IntegerOverflow(offset) => {
                    FuncError::inst(bytecode, offset, InstError::IntegerOverflow)
                }
                _ => unreachable!(),
            })?;
            let InstOffsets {
                before,
                inst,
                after,
            } = last;
            self.offsets.push(OffsetInfo {
                offset: before,
                start_bb: need_bb,
            });
            need_bb = false;

            match inst {
                // Nothing to validate syntactically
                Add | Div | Ediv | Dup | Mod | Mul | Neg | Not | BitNot | BitAnd | BitOr
                | BitXor | Pop | Eq | Ne | Gt | Lt | Gte | Lte | Sub | Ret | PopUpvars | Index
                | Assign | Next => (),
                // Must wait for data flow to interpret these immediates
                LoadUpvar(_, _) | StoreUpvar(_, _) | PushUpvars(_) => (),
                // Disallow swaps that don't do anything, in case the implementation is unsafe with
                // overlapping targets
                Swap(i, j) => {
                    if i == j {
                        return Err(FuncError::inst(bytecode, before, InstError::NoOpSwap));
                    }
                }
                // Call signature must be valid
                Call(index) => self.ctx.pack(index).map(|_| ()).ok_or(FuncError::inst(
                    bytecode,
                    before,
                    InstError::PackIndexOutOfBounds,
                ))?,
                // Symbol, call signature must be valid
                MethodCall(sym, sig) => {
                    if !self.ctx.symbol_valid(sym) {
                        return Err(FuncError::inst(
                            bytecode,
                            before,
                            InstError::SymIndexOutOfBounds,
                        ));
                    }
                    self.ctx.pack(sig).map(|_| ()).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::PackIndexOutOfBounds,
                    ))?
                }
                // Index, call signature must be valid
                Builtin(idx, sig) => {
                    if idx >= BUILTINS.len() {
                        return Err(FuncError::inst(
                            bytecode,
                            before,
                            InstError::BuiltinIndexOutOfBounds,
                        ));
                    }
                    self.ctx.pack(sig).map(|_| ()).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::PackIndexOutOfBounds,
                    ))?
                }
                // Constant index must be valid
                LoadConst(index) => {
                    if !self.ctx.constant_valid(index) {
                        return Err(FuncError::inst(
                            bytecode,
                            before,
                            InstError::ConstIndexOutOfBounds,
                        ));
                    }
                }
                // Local index must be in bounds
                LoadLocal(index) | StoreLocal(index) => {
                    if index >= self.func.locals {
                        return Err(FuncError::inst(
                            bytecode,
                            before,
                            InstError::LocalIndexOutOfBounds,
                        ));
                    }
                }
                // Get/set symbols must be valid
                Get(index) | Set(index) => {
                    if !self.ctx.symbol_valid(index) {
                        return Err(FuncError::inst(
                            bytecode,
                            before,
                            InstError::SymIndexOutOfBounds,
                        ));
                    }
                }
                // Closure function index must be valid;
                // upvar compatibility checked during data flow
                Close(index) => {
                    self.ctx.function(index).map(|_| ()).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::FuncIndexOutOfBounds,
                    ))?;
                    // Note that we use function
                    self.used[self.index].uses.insert(index);
                }
                // Branch targets must be in bounds; targets being valid instruction offsets
                // is verified in the next pass
                Branch(target) | BranchTrue(target) | BranchFalse(target) => {
                    after
                        .checked_add_signed(target)
                        .and_then(|abs| if abs >= bytecode_len { None } else { Some(()) })
                        .ok_or(FuncError::inst(
                            bytecode,
                            before,
                            InstError::BranchTargetOutOfBounds,
                        ))?;
                    // Note that next instruction should start a basic block
                    need_bb = true;
                }
                // Pack index must be in bounds
                Reify(index) => {
                    let _ = self.ctx.pack(index).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::PackIndexOutOfBounds,
                    ))?;
                }
                // Unpack index must be in bounds
                Unpack(index) => {
                    self.ctx.unpack_arity(index).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::UnpackIndexOutOfBounds,
                    ))?;
                }
                // Function index must be valid; marks use relationship
                NlGuard(index) => {
                    self.ctx.function(index).map(|_| ()).ok_or(FuncError::inst(
                        bytecode,
                        before,
                        InstError::FuncIndexOutOfBounds,
                    ))?;
                    self.used[self.index].uses.insert(index);
                }
                // Upvar depth/indicator checked during data flow;
                // terminal instruction
                NlBranch(_, _) => {
                    need_bb = true;
                }
            }
        }
        match last.inst {
            Ret | Branch(_) | BranchTrue(_) | BranchFalse(_) | NlBranch(_, _) => (),
            _ => return Err(FuncError::UnexpectedEnd),
        }
        Ok(())
    }

    // Pass 1: branch target validity => basic blocks
    fn basic_blocks(&mut self, cert: Option<&Certificate>) -> FuncResult<()> {
        use Inst::*;

        let bytecode = self.ctx.slice(&self.func.bytecode);

        for item in Self::insts_verified(bytecode, 0) {
            let InstOffsets {
                before,
                inst,
                after,
            } = item;
            match inst {
                // Branch targets must be the start of an instruction
                Branch(target) | BranchTrue(target) | BranchFalse(target) => {
                    let offset = after.strict_add_signed(target);
                    match self.offsets.binary_search_by_key(
                        &offset,
                        |OffsetInfo {
                             offset,
                             start_bb: _,
                         }| *offset,
                    ) {
                        // Note that this instruction should start a basic block
                        Ok(index) => self.offsets[index].start_bb = true,
                        Err(_) => {
                            return Err(FuncError::inst(
                                bytecode,
                                before,
                                InstError::BranchTargetInvalid,
                            ));
                        }
                    }
                }
                _ => (),
            }
        }

        // If we have an input certificate, it must agree with us about basic block placement
        if let Some(cert) = cert {
            // Collect all basic block start offsets
            let offsets: Vec<_> = self
                .offsets
                .iter()
                .filter_map(
                    |OffsetInfo { offset, start_bb }| if *start_bb { Some(*offset) } else { None },
                )
                .collect();
            // Certificate should have the same offsets, except that the entry block is
            // omitted from the certificate because its flow state is known from its signature
            if cert.blocks.len() != offsets.len() - 1
                || !offsets
                    .iter()
                    .skip(1)
                    .zip(cert.blocks.iter())
                    .all(|(b, c)| *b == c.0)
            {
                return Err(FuncError::BasicBlockMismatch);
            }

            // All is well, initialize blocks from certificate.  Entry block state will be set
            // up by data flow pass.
            self.blocks.push(Block {
                offset: 0,
                mark: false,
                state: BlockState::neutral(),
            });
            self.blocks
                .extend(cert.blocks.iter().map(|(offset, state)| Block {
                    offset: *offset,
                    mark: false,
                    state: state.clone(),
                }));
        } else {
            // Initialize blocks to neutral state
            for OffsetInfo {
                offset,
                start_bb: bb,
            } in self.offsets.iter()
            {
                if *bb {
                    self.blocks.push(Block {
                        offset: *offset,
                        mark: false,
                        state: BlockState::neutral(),
                    });
                }
            }
        }

        Ok(())
    }

    // Model effect of single instruction on block state
    fn step(&mut self, block: &mut BlockState, inst: &Inst) -> FlowResult<()> {
        use Inst::*;

        match inst {
            Branch(_) => (),
            Neg | Not | BitNot => {
                block.pop()?;
                block.push()
            }
            Add | Div | Ediv | Mod | Mul | BitAnd | BitOr | BitXor | Eq | Ne | Gt | Lt | Gte
            | Lte | Sub => {
                block.pop()?;
                block.pop()?;
                block.push()
            }
            Dup => {
                block.pop()?;
                block.push();
                block.push();
            }
            Swap(i, j) => {
                if *i >= block.operands {
                    return Err(InstError::OperandIndexOutOfBounds(*i));
                }
                if *j >= block.operands {
                    return Err(InstError::OperandIndexOutOfBounds(*j));
                }
            }
            Pop | BranchTrue(_) | BranchFalse(_) => {
                block.pop()?;
            }
            LoadLocal(index) => {
                if block.locals[*index] == LocalState::Invalid {
                    return Err(InstError::LocalLoadUninitialized);
                }
                block.push()
            }
            StoreLocal(index) => {
                block.pop()?;
                block.locals[*index] = LocalState::Value
            }
            Call(index) | MethodCall(_, index) => {
                let arity = self.ctx.pack(*index).unwrap().count();
                for _ in 0..arity {
                    block.pop()?;
                }
                block.pop()?;
                block.push()
            }
            Builtin(_, sig) => {
                let arity = self.ctx.pack(*sig).unwrap().count();
                for _ in 0..arity {
                    block.pop()?;
                }
                block.push()
            }
            LoadConst(_) => block.push(),
            LoadUpvar(index, depth) | StoreUpvar(index, depth) => {
                if *depth >= block.upvars.len() {
                    return Err(InstError::UpvarDepthOutOfBounds);
                }
                if *index >= block.upvars[block.upvars.len() - 1 - *depth] {
                    return Err(InstError::UpvarIndexOutOfBounds);
                }
                if matches!(inst, LoadUpvar(..)) {
                    block.push()
                } else {
                    block.pop()?
                }
            }
            Get(_) => {
                block.pop()?;
                block.push()
            }
            Set(_) => {
                block.pop()?;
                block.pop()?
            }
            Index => {
                block.pop()?;
                block.pop()?;
                block.push();
            }
            Assign => {
                block.pop()?;
                block.pop()?;
                block.pop()?;
            }
            PushUpvars(count) => {
                block.upvars.push(*count);
                // Enforce static upvar limit
                if block
                    .upvars
                    .iter()
                    .fold(0usize, |acc, count| acc.saturating_add(*count))
                    > limit::UPVAR_TOTAL
                {
                    return Err(InstError::UpvarPushExceedsLimit);
                }
            }
            PopUpvars => {
                if block.upvars.len() == self.func.upvars.len() {
                    return Err(InstError::UpvarUnderflow);
                }
                block.upvars.pop();
            }
            Close(index) => {
                let func = self
                    .ctx
                    .function(*index)
                    .ok_or(InstError::FuncIndexOutOfBounds)?;
                if func.upvars != block.upvars {
                    return Err(InstError::UpvarSigMismatch);
                }
                block.push()
            }
            Ret => {
                block.pop()?;
                if block.operands != 0 {
                    return Err(InstError::ReturnWithExcessOperands);
                }
                if block.upvars.len() != self.func.upvars.len() {
                    return Err(InstError::ReturnWithExcessUpvars);
                }
            }
            Reify(index) => {
                if block.upvars.len() == self.func.upvars.len() {
                    return Err(InstError::UpvarUnderflow);
                }
                let count = block.upvars.pop().unwrap();
                let arity = self.ctx.pack(*index).unwrap().count();
                if count != arity {
                    return Err(InstError::ReifyMismatch(count, arity));
                }
                block.push()
            }
            Next => {
                block.pop()?;
                block.push();
                block.push();
            }
            Unpack(index) => {
                let arity = self.ctx.unpack_arity(*index).unwrap();
                block.pop()?;
                for _ in 0..arity {
                    block.push();
                }
            }
            NlGuard(index) => {
                let func = self
                    .ctx
                    .function(*index)
                    .ok_or(InstError::FuncIndexOutOfBounds)?;
                block.upvars.push(0);
                if func.upvars != block.upvars {
                    return Err(InstError::UpvarSigMismatch);
                }
                block.upvars.pop();
                // Pushes 2 values: result and indicator
                block.push();
                block.push();
            }
            NlBranch(depth, _) => {
                // Terminal: validates upvar depth is in bounds
                if *depth >= block.upvars.len() {
                    return Err(InstError::UpvarDepthOutOfBounds);
                }
            }
        }

        Ok(())
    }

    // Pass 2: dataflow analysis
    fn dataflow(&mut self, cert: Option<&Certificate>) -> FuncResult<()> {
        use Inst::*;

        let bytecode = self.ctx.slice(&self.func.bytecode);

        // Initialize entry block state from function signature
        let operands = self
            .ctx
            .unpack_arity(self.func.sig)
            // Basic file verification should have caught this
            .expect("no unpack table entry for function");

        self.blocks[0].state = BlockState {
            operands,
            locals: vec![LocalState::Invalid; self.func.locals],
            upvars: self.func.upvars.clone(),
        };
        self.blocks[0].mark = true;

        if operands > self.max_operand_depth {
            self.max_operand_depth = operands
        }

        self.queue.push(0);

        while let Some(bb) = self.queue.pop() {
            let start = self.blocks[bb].offset;
            let end = self.blocks.get(bb + 1).map(|b| b.offset);
            let mut block = self.blocks[bb].state.clone();
            // Restrict bytecode slice to end of basic block
            let slice = if let Some(end) = end {
                &bytecode[0..end]
            } else {
                bytecode
            };

            // If we're generating a certificate, mark indicates whether block has changed state
            if cert.is_none() {
                self.blocks[bb].mark = false;
            }

            #[cfg(feature = "debug")]
            eprintln!("  bb #{bb}: {}", block);

            // Dummy value, immediately overwritten
            let mut last = InstOffsets {
                before: 0usize,
                inst: Inst::Pop,
                after: 0usize,
            };

            // Compute width of hex PC offsets for debug output
            #[cfg(feature = "debug")]
            let width = ((bytecode.len() - 1).max(1).ilog2() + 1).div_ceil(4).max(2) as usize;

            // Step over all instructions in block
            for item in Self::insts_verified(slice, start) {
                last = item;
                #[cfg(feature = "debug")]
                eprintln!("    {:0width$x} {}", last.before, last.inst);
                self.step(&mut block, &last.inst)
                    .map_err(|e| FuncError::inst(slice, last.before, e))?;
                #[cfg(feature = "debug")]
                eprintln!("      ⮡ {}", block);
                if block.operands > self.max_operand_depth {
                    self.max_operand_depth = block.operands
                }
            }

            // Immediate fall-through successor block, if applicable
            let next = match last.inst {
                Branch(_) | Ret | NlBranch(_, _) => None,
                _ => {
                    if bb + 1 != self.blocks.len() {
                        Some(bb + 1)
                    } else {
                        None
                    }
                }
            };

            // Branch target successor block, if applicable
            let branch = match last.inst {
                Branch(index) | BranchTrue(index) | BranchFalse(index) => Some(
                    self.blocks
                        .binary_search_by_key(
                            &last.after.strict_add_signed(index),
                            |Block {
                                 offset,
                                 mark: _,
                                 state: _,
                             }| { *offset },
                        )
                        .unwrap(),
                ),
                _ => None,
            };

            // Merge state into each sucessor block,
            for sid in next.into_iter().chain(branch) {
                let succ = &mut self.blocks[sid];
                if cert.is_some() {
                    // Certificate check case
                    // Mark block as reached
                    if !succ.mark {
                        succ.mark = true;
                        #[cfg(feature = "debug")]
                        eprintln!("    mark #{}", sid);
                        self.queue.push(sid);
                    }
                    // Check that claimed successor state is unchanged by merge
                    // (and that merge doesn't otherwise catch an invariant violation)
                    let mut claimed = succ.state.clone();
                    block
                        .merge(&mut claimed)
                        .map_err(|e| FuncError::inst(bytecode, succ.offset, e))?;
                    if claimed != succ.state {
                        return Err(FuncError::inst(
                            bytecode,
                            succ.offset,
                            InstError::NotAFixedPoint,
                        ));
                    }
                } else if succ.mark {
                    // Certificate generation case, block is already marked for visit
                    // Just merge predecessor state into it
                    #[cfg(feature = "debug")]
                    eprintln!("    merge #{}", sid);
                    block
                        .merge(&mut succ.state)
                        .map_err(|e| FuncError::inst(bytecode, succ.offset, e))?
                } else {
                    // Certificate generation case, block is not marked for visit
                    // Merge state into it, check for change
                    let prev = succ.state.clone();
                    block
                        .merge(&mut succ.state)
                        .map_err(|e| FuncError::inst(bytecode, succ.offset, e))?;
                    // If the state did change, mark and queue block for visit
                    if succ.state != prev {
                        #[cfg(feature = "debug")]
                        eprintln!("    changed #{}", sid);
                        succ.mark = true;
                        self.queue.push(sid);
                    }
                }
            }
        }

        // Verify all blocks were reached.  Syntactically unreachable blocks aren't unsafe per se,
        // but could indicate a heap/JIT spray attack. We could also perform this check during
        // certificate generation as a sanity test of the lowering pass, but don't at present.
        if cert.is_some() {
            for block in self.blocks.iter() {
                if !block.mark {
                    return Err(FuncError::inst(
                        bytecode,
                        block.offset,
                        InstError::InstructionUnreachable,
                    ));
                }
            }
        }

        Ok(())
    }

    // Compute certificate from scratch
    pub fn compute(mut self) -> FuncResult<Certificate> {
        self.syntax()?;
        self.basic_blocks(None)?;
        self.dataflow(None)?;

        Ok(Certificate {
            max_operand_depth: self.max_operand_depth,
            blocks: self
                .blocks
                .into_iter()
                .skip(1)
                .map(|b| (b.offset, b.state))
                .collect(),
        })
    }

    // Check existing certificate is legitimate
    pub fn check(&mut self, cert: &Certificate) -> FuncResult<()> {
        self.syntax()?;
        self.basic_blocks(Some(cert))?;
        self.dataflow(Some(cert))?;

        // Verify the certificate has the correct max operand stack depth
        if cert.max_operand_depth != self.max_operand_depth {
            return Err(FuncError::OperandDepthMismatch);
        }

        Ok(())
    }
}

pub struct Verifier<'a, C: Context> {
    // Verification context
    ctx: &'a C,
}

impl<'a, C: Context> Verifier<'a, C> {
    pub fn new(ctx: &'a C) -> Self {
        Self { ctx }
    }

    // Verify that all functions are transitively reachable from entry function by use in closure
    // construction.  This isn't unsafe per se, but could indicate a (lazy) attempt at heap/JIT
    // spraying.
    fn verify_used(used: &mut [Used]) -> Result<()> {
        let mut queue = Vec::new();

        // Entry function is always used
        used[0].used = true;
        queue.push(0);

        while let Some(index) = queue.pop() {
            for reli in used[index].uses.clone().into_iter() {
                let rel = &mut used[reli];
                if !rel.used {
                    rel.used = true;
                    queue.push(reli);
                }
            }
        }

        for (i, used) in used.iter().enumerate() {
            if !used.used {
                return Err(Error {
                    func: i,
                    error: FuncError::Unused,
                });
            }
        }

        Ok(())
    }

    pub fn compute(
        &self,
        funcs: impl IntoIterator<Item = &'a Func<C::Phase>>,
    ) -> Result<Box<[Certificate]>> {
        let mut certs = Vec::new();
        let funcs: Vec<_> = funcs.into_iter().collect();
        let mut used = vec![Default::default(); funcs.len()];
        for (i, func) in funcs.into_iter().enumerate() {
            #[cfg(feature = "debug")]
            eprintln!("Compute cert #{i}:");
            let verifier = FuncVerifier::new(self.ctx, i, func, &mut used);
            certs.push(
                verifier
                    .compute()
                    .map_err(|e| Error { func: i, error: e })?,
            );
        }

        Self::verify_used(&mut used)?;

        Ok(certs.into())
    }

    pub fn check(
        &self,
        funcs: impl IntoIterator<Item = (&'a Func<C::Phase>, &'a Certificate)>,
    ) -> Result<()> {
        let funcs: Vec<_> = funcs.into_iter().collect();
        let mut used = vec![Default::default(); funcs.len()];
        for (i, (func, cert)) in funcs.into_iter().enumerate() {
            #[cfg(feature = "debug")]
            eprintln!("Check cert #{i}:");
            let mut verifier = FuncVerifier::new(self.ctx, i, func, &mut used);
            verifier
                .check(cert)
                .map_err(|e| Error { func: i, error: e })?
        }

        Self::verify_used(&mut used)?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::super::{Encode, Func, Opcode};
    use super::*;

    struct Mock {
        packs: Vec<Vec<Arg>>,
        unpack_arities: Vec<usize>,
        symbols: usize,
        constants: usize,
    }

    struct Input {
        sig: usize,
        locals: usize,
        upvars: Vec<usize>,
        bytecode: Box<[u8]>,
    }

    struct Context {
        mock: Mock,
        funcs: Vec<Func<Test>>,
    }

    struct Test;

    impl Phase for Test {
        type Bytes = Box<[u8]>;
    }

    impl super::Context for Context {
        type Phase = Test;

        fn slice<'a>(&'a self, bytes: &'a <Self::Phase as Phase>::Bytes) -> &'a [u8] {
            bytes
        }

        fn function(&self, index: usize) -> Option<&Func<Self::Phase>> {
            self.funcs.get(index)
        }

        fn pack(&self, index: usize) -> Option<impl Iterator<Item = Arg>> {
            self.mock.packs.get(index).map(|v| v.iter().cloned())
        }

        fn unpack_arity(&self, index: usize) -> Option<usize> {
            self.mock.unpack_arities.get(index).copied()
        }

        fn symbol_valid(&self, index: usize) -> bool {
            index < self.mock.symbols
        }

        fn constant_valid(&self, index: usize) -> bool {
            index < self.mock.constants
        }
    }

    fn assemble(insts: &[Inst]) -> Box<[u8]> {
        let mut offsets = vec![0; insts.len() + 1];
        let mut bytecode = Vec::new();
        let mut changing = true;

        while changing {
            changing = false;
            bytecode.clear();

            for (i, inst) in insts.iter().enumerate() {
                let offset = bytecode.len();
                if offsets[i] != offset {
                    changing = true;
                    offsets[i] = offset
                }
                match inst {
                    Inst::Branch(t) | Inst::BranchTrue(t) | Inst::BranchFalse(t) => {
                        let offset = offsets[*t as usize] as isize - offsets[i + 1] as isize;
                        match inst {
                            Inst::Branch(_) => Inst::Branch(offset).encode(&mut bytecode).unwrap(),
                            Inst::BranchTrue(_) => {
                                Inst::BranchTrue(offset).encode(&mut bytecode).unwrap()
                            }
                            Inst::BranchFalse(_) => {
                                Inst::BranchFalse(offset).encode(&mut bytecode).unwrap()
                            }
                            _ => unreachable!(),
                        }
                    }
                    _ => inst.encode(&mut bytecode).unwrap(),
                }
            }

            if offsets[insts.len()] != bytecode.len() {
                offsets[insts.len()] = bytecode.len();
                changing = true
            }
        }

        bytecode.into()
    }

    fn encode_raw(insts: &[Inst]) -> Box<[u8]> {
        let mut bytecode = Vec::new();
        for inst in insts {
            inst.encode(&mut bytecode).unwrap();
        }
        bytecode.into()
    }

    fn link(mock: Mock, funcs: Vec<Input>) -> Context {
        Context {
            mock,
            funcs: funcs
                .into_iter()
                .map(
                    |Input {
                         sig,
                         locals,
                         upvars,
                         bytecode,
                     }| Func {
                        bytecode,
                        sig,
                        locals,
                        upvars,
                    },
                )
                .collect(),
        }
    }

    fn func(locals: usize, upvars: Vec<usize>, insts: Vec<Inst>) -> Input {
        Input {
            sig: 0,
            locals,
            upvars,
            bytecode: assemble(&insts),
        }
    }

    fn raw_func(locals: usize, upvars: Vec<usize>, insts: Vec<Inst>) -> Input {
        Input {
            sig: 0,
            locals,
            upvars,
            bytecode: encode_raw(&insts),
        }
    }

    fn run(ctx: &Context) -> super::Result<Box<[Certificate]>> {
        let verifier = Verifier::new(ctx);
        let certs = verifier.compute(ctx.funcs.iter())?;
        verifier.check(ctx.funcs.iter().zip(certs.iter()))?;
        Ok(certs)
    }

    fn inst_error(ctx: Context, expect: InstError) {
        let res = run(&ctx);
        match res {
            Err(
                ref e @ Error {
                    error: FuncError::Inst(_, ref got, _),
                    ..
                },
            ) if expect == *got => {
                eprintln!("expected error: {e}");
            }
            _ => panic!("unexpected result: {res:?}"),
        }
    }

    fn func_error(ctx: Context, expect: FuncError) {
        let res = run(&ctx);
        match res {
            Err(ref e @ Error { error: ref got, .. }) if expect == *got => {
                eprintln!("expected error: {e}");
            }
            _ => panic!("unexpected result: {res:?}"),
        }
    }

    fn check_error(ctx: &Context, certs: &[Certificate], expect: FuncError) {
        let res = Verifier::new(ctx).check(ctx.funcs.iter().zip(certs.iter()));
        match res {
            Err(ref e @ Error { error: ref got, .. }) if expect == *got => {
                eprintln!("expected error: {e}");
            }
            _ => panic!("unexpected result: {res:?}"),
        }
    }

    fn default_mock() -> Mock {
        Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        }
    }

    #[test]
    fn pc_cliff() {
        use Inst::*;

        let mock = default_mock();
        let inputs = vec![func(0, vec![], vec![LoadConst(0)])];

        func_error(link(mock, inputs), FuncError::UnexpectedEnd);
    }

    #[test]
    fn empty_pop() {
        use Inst::*;

        let mock = Mock {
            constants: 0,
            ..default_mock()
        };
        let inputs = vec![func(0, vec![], vec![Pop, Ret])];

        inst_error(link(mock, inputs), InstError::OperandUnderflow);
    }

    #[test]
    fn ret_excessive_operands() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        };

        let inputs = vec![func(0, vec![], vec![LoadConst(0), Dup, Ret])];

        inst_error(link(mock, inputs), InstError::ReturnWithExcessOperands);
    }

    #[test]
    fn ret_excessive_upvars() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        };

        let inputs = vec![func(0, vec![], vec![LoadConst(0), PushUpvars(1), Ret])];

        inst_error(link(mock, inputs), InstError::ReturnWithExcessUpvars);
    }

    #[test]
    fn operand_overflow() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        };

        let inputs = vec![func(0, vec![], vec![LoadConst(0), Branch(0)])];

        inst_error(
            link(mock, inputs),
            InstError::OperandDepthConflict { from: 1, into: 0 },
        );
    }

    #[test]
    fn noop_swap() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        };

        let inputs = vec![func(0, vec![], vec![LoadConst(0), Swap(0, 0), Ret])];

        inst_error(link(mock, inputs), InstError::NoOpSwap);
    }

    #[test]
    fn const_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 1,
        };

        let inputs = vec![func(0, vec![], vec![LoadConst(1), Ret])];

        inst_error(link(mock, inputs), InstError::ConstIndexOutOfBounds);
    }

    #[test]
    fn sym_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 1,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![Get(1), Ret])];

        inst_error(link(mock, inputs), InstError::SymIndexOutOfBounds);
    }

    #[test]
    fn builtin_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![vec![]],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![Builtin(100, 0), Ret])];

        inst_error(link(mock, inputs), InstError::BuiltinIndexOutOfBounds);
    }

    #[test]
    fn local_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(1, vec![], vec![LoadLocal(1), Ret])];

        inst_error(link(mock, inputs), InstError::LocalIndexOutOfBounds);
    }

    #[test]
    fn local_load_uninitialized() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(1, vec![], vec![LoadLocal(0), Ret])];

        inst_error(link(mock, inputs), InstError::LocalLoadUninitialized);
    }

    #[test]
    fn upvar_depth_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![LoadUpvar(0, 1), Ret])];

        inst_error(link(mock, inputs), InstError::UpvarDepthOutOfBounds);
    }

    #[test]
    fn upvar_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![1], vec![LoadUpvar(2, 0), Ret])];

        inst_error(link(mock, inputs), InstError::UpvarIndexOutOfBounds);
    }

    #[test]
    fn pack_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![vec![]],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![Call(1), Ret])];

        inst_error(link(mock, inputs), InstError::PackIndexOutOfBounds);
    }

    #[test]
    fn unpack_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![Unpack(1), Ret])];

        inst_error(link(mock, inputs), InstError::UnpackIndexOutOfBounds);
    }

    #[test]
    fn func_index_out_of_bounds() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![Close(1), Ret])];

        inst_error(link(mock, inputs), InstError::FuncIndexOutOfBounds);
    }

    #[test]
    fn upvar_sig_mismatch() {
        use Inst::*;

        let mock = Mock {
            packs: vec![],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![PushUpvars(1), Close(0), Ret])];

        inst_error(link(mock, inputs), InstError::UpvarSigMismatch);
    }

    #[test]
    fn reify_mismatch() {
        use super::super::Arg;
        use Inst::*;

        let mock = Mock {
            packs: vec![vec![Arg::Value, Arg::Value]],
            unpack_arities: vec![0],
            symbols: 0,
            constants: 0,
        };

        let inputs = vec![func(0, vec![], vec![PushUpvars(1), Reify(0), Ret])];

        inst_error(link(mock, inputs), InstError::ReifyMismatch(1, 2));
    }

    #[test]
    fn valid_straight_line_compute_and_check() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(0, vec![], vec![LoadConst(0), Ret])],
        );
        run(&ctx).unwrap();
    }

    #[test]
    fn valid_branching_compute_and_check() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    BranchTrue(4),
                    LoadConst(0),
                    Branch(5),
                    LoadConst(0),
                    Ret,
                ],
            )],
        );
        run(&ctx).unwrap();
    }

    #[test]
    fn valid_multifunc_reachability() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![
                func(0, vec![], vec![Close(1), Ret]),
                func(0, vec![], vec![Close(2), Ret]),
                func(0, vec![], vec![LoadConst(0), Ret]),
            ],
        );
        run(&ctx).unwrap();
    }

    #[test]
    fn valid_local_init_path() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                1,
                vec![],
                vec![LoadConst(0), StoreLocal(0), LoadLocal(0), Ret],
            )],
        );
        run(&ctx).unwrap();
    }

    #[test]
    fn valid_method_call_builtin_and_unpack_paths() {
        use Inst::*;

        let ctx = link(
            Mock {
                packs: vec![vec![Arg::Value]],
                unpack_arities: vec![0, 1],
                symbols: 1,
                constants: 1,
            },
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    LoadConst(0),
                    MethodCall(0, 0),
                    Builtin(crate::builtin::THROW, 0),
                    Unpack(1),
                    Ret,
                ],
            )],
        );
        run(&ctx).unwrap();
    }

    #[test]
    fn valid_get_set_index_assign_next_and_nonlocal_paths() {
        use Inst::*;

        let ctx = link(
            Mock {
                packs: vec![],
                unpack_arities: vec![0],
                symbols: 1,
                constants: 1,
            },
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    Get(0),
                    LoadConst(0),
                    Set(0),
                    LoadConst(0),
                    LoadConst(0),
                    Index,
                    Pop,
                    LoadConst(0),
                    LoadConst(0),
                    LoadConst(0),
                    Assign,
                    LoadConst(0),
                    Next,
                    Pop,
                    Ret,
                ],
            )],
        );
        run(&ctx).unwrap();

        let nonlocal = link(
            Mock {
                packs: vec![],
                unpack_arities: vec![0],
                symbols: 0,
                constants: 1,
            },
            vec![
                func(0, vec![], vec![NlGuard(1), Pop, Pop, LoadConst(0), Ret]),
                func(0, vec![0], vec![LoadConst(0), Ret]),
            ],
        );
        run(&nonlocal).unwrap();

        let nlbranch_only = link(
            Mock {
                packs: vec![],
                unpack_arities: vec![0],
                symbols: 0,
                constants: 1,
            },
            vec![func(0, vec![1], vec![NlBranch(0, 0)])],
        );
        run(&nlbranch_only).unwrap();
    }

    #[test]
    fn invalid_opcode() {
        let ctx = link(
            Mock {
                constants: 0,
                ..default_mock()
            },
            vec![Input {
                sig: 0,
                locals: 0,
                upvars: vec![],
                bytecode: vec![0].into(),
            }],
        );

        inst_error(ctx, InstError::InvalidOpcode);
    }

    #[test]
    fn truncated_instruction() {
        let ctx = link(
            Mock {
                constants: 0,
                ..default_mock()
            },
            vec![Input {
                sig: 0,
                locals: 0,
                upvars: vec![],
                bytecode: vec![Opcode::Call as u8].into(),
            }],
        );

        inst_error(ctx, InstError::Truncated);
    }

    #[test]
    fn branch_target_out_of_bounds() {
        use Inst::*;

        let ctx = link(default_mock(), vec![raw_func(0, vec![], vec![Branch(99)])]);
        inst_error(ctx, InstError::BranchTargetOutOfBounds);
    }

    #[test]
    fn branch_target_invalid() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![raw_func(0, vec![], vec![LoadConst(0), Branch(-3)])],
        );
        inst_error(ctx, InstError::BranchTargetInvalid);
    }

    #[test]
    fn method_call_symbol_out_of_bounds() {
        use Inst::*;

        let ctx = link(
            Mock {
                packs: vec![vec![Arg::Value]],
                unpack_arities: vec![0],
                symbols: 1,
                constants: 1,
            },
            vec![func(
                0,
                vec![],
                vec![LoadConst(0), LoadConst(0), MethodCall(1, 0), Ret],
            )],
        );
        inst_error(ctx, InstError::SymIndexOutOfBounds);
    }

    #[test]
    fn builtin_pack_index_out_of_bounds() {
        use Inst::*;

        let ctx = link(
            Mock {
                packs: vec![],
                unpack_arities: vec![0],
                symbols: 0,
                constants: 1,
            },
            vec![func(
                0,
                vec![],
                vec![LoadConst(0), Builtin(crate::builtin::ARGS, 0), Ret],
            )],
        );
        inst_error(ctx, InstError::PackIndexOutOfBounds);
    }

    #[test]
    fn reify_pack_index_out_of_bounds() {
        use Inst::*;

        let ctx = link(
            Mock {
                packs: vec![],
                unpack_arities: vec![0],
                symbols: 0,
                constants: 1,
            },
            vec![func(0, vec![], vec![PushUpvars(1), Reify(0), Ret])],
        );
        inst_error(ctx, InstError::PackIndexOutOfBounds);
    }

    #[test]
    fn nlguard_func_index_out_of_bounds() {
        use Inst::*;

        let ctx = link(default_mock(), vec![func(0, vec![], vec![NlGuard(1), Ret])]);
        inst_error(ctx, InstError::FuncIndexOutOfBounds);
    }

    #[test]
    fn upvar_limit_exceeded_in_signature() {
        let ctx = link(
            default_mock(),
            vec![func(0, vec![limit::UPVAR_TOTAL + 1], vec![Inst::Ret])],
        );
        func_error(ctx, FuncError::UpvarLimitExceeded);
    }

    #[test]
    fn swap_operand_index_out_of_bounds() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(0, vec![], vec![LoadConst(0), Swap(1, 0), Ret])],
        );
        inst_error(ctx, InstError::OperandIndexOutOfBounds(1));
    }

    #[test]
    fn store_local_operand_underflow() {
        use Inst::*;

        let ctx = link(
            Mock {
                constants: 0,
                ..default_mock()
            },
            vec![func(1, vec![], vec![StoreLocal(0), Ret])],
        );
        inst_error(ctx, InstError::OperandUnderflow);
    }

    #[test]
    fn store_upvar_operand_underflow() {
        use Inst::*;

        let ctx = link(
            Mock {
                constants: 0,
                ..default_mock()
            },
            vec![func(0, vec![1], vec![StoreUpvar(0, 0), Ret])],
        );
        inst_error(ctx, InstError::OperandUnderflow);
    }

    #[test]
    fn unary_and_binary_operand_underflow() {
        use Inst::*;

        inst_error(
            link(
                Mock {
                    constants: 0,
                    ..default_mock()
                },
                vec![func(0, vec![], vec![Neg, Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                Mock {
                    constants: 0,
                    ..default_mock()
                },
                vec![func(0, vec![], vec![Add, Ret])],
            ),
            InstError::OperandUnderflow,
        );
    }

    #[test]
    fn get_set_index_and_assign_underflow() {
        use Inst::*;

        inst_error(
            link(
                Mock {
                    symbols: 1,
                    constants: 0,
                    ..default_mock()
                },
                vec![func(0, vec![], vec![Get(0), Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                Mock {
                    symbols: 1,
                    constants: 1,
                    ..default_mock()
                },
                vec![func(0, vec![], vec![LoadConst(0), Set(0), Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                default_mock(),
                vec![func(0, vec![], vec![LoadConst(0), Index, Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                default_mock(),
                vec![func(
                    0,
                    vec![],
                    vec![LoadConst(0), LoadConst(0), Assign, Ret],
                )],
            ),
            InstError::OperandUnderflow,
        );
    }

    #[test]
    fn call_method_and_builtin_operand_underflow() {
        use Inst::*;

        inst_error(
            link(
                Mock {
                    packs: vec![vec![Arg::Value]],
                    unpack_arities: vec![0],
                    symbols: 0,
                    constants: 1,
                },
                vec![func(0, vec![], vec![LoadConst(0), Call(0), Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                Mock {
                    packs: vec![vec![Arg::Value]],
                    unpack_arities: vec![0],
                    symbols: 1,
                    constants: 1,
                },
                vec![func(0, vec![], vec![LoadConst(0), MethodCall(0, 0), Ret])],
            ),
            InstError::OperandUnderflow,
        );
        inst_error(
            link(
                Mock {
                    packs: vec![vec![Arg::Value]],
                    unpack_arities: vec![0],
                    symbols: 0,
                    constants: 0,
                },
                vec![func(0, vec![], vec![Builtin(crate::builtin::ARGS, 0), Ret])],
            ),
            InstError::OperandUnderflow,
        );
    }

    #[test]
    fn popupvars_underflow() {
        use Inst::*;

        let ctx = link(default_mock(), vec![func(0, vec![], vec![PopUpvars, Ret])]);
        inst_error(ctx, InstError::UpvarUnderflow);
    }

    #[test]
    fn push_upvars_exceeds_limit() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![PushUpvars(limit::UPVAR_TOTAL + 1), LoadConst(0), Ret],
            )],
        );
        inst_error(ctx, InstError::UpvarPushExceedsLimit);
    }

    #[test]
    fn nlbranch_depth_out_of_bounds() {
        use Inst::*;

        let ctx = link(
            Mock {
                constants: 0,
                ..default_mock()
            },
            vec![func(0, vec![], vec![NlBranch(0, 0)])],
        );
        inst_error(ctx, InstError::UpvarDepthOutOfBounds);
    }

    #[test]
    fn upvar_depth_conflict() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    BranchTrue(5),
                    PushUpvars(1),
                    LoadConst(0),
                    Branch(6),
                    LoadConst(0),
                    Ret,
                ],
            )],
        );
        inst_error(ctx, InstError::UpvarDepthConflict { from: 1, into: 0 });
    }

    #[test]
    fn upvar_count_conflict() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    BranchTrue(4),
                    PushUpvars(1),
                    Branch(6),
                    PushUpvars(2),
                    Branch(6),
                    PopUpvars,
                    LoadConst(0),
                    Ret,
                ],
            )],
        );
        inst_error(
            ctx,
            InstError::UpvarCountConflict {
                depth: 0,
                from: 1,
                into: 2,
            },
        );
    }

    #[test]
    fn unused_function_is_rejected() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![
                func(0, vec![], vec![LoadConst(0), Ret]),
                func(0, vec![], vec![LoadConst(0), Ret]),
            ],
        );
        func_error(ctx, FuncError::Unused);
    }

    #[test]
    fn instruction_unreachable_during_check() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![LoadConst(0), Branch(4), LoadConst(0), Pop, Ret],
            )],
        );
        inst_error(ctx, InstError::InstructionUnreachable);
    }

    #[test]
    fn certificate_basic_block_mismatch() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    BranchTrue(4),
                    LoadConst(0),
                    Branch(5),
                    LoadConst(0),
                    Ret,
                ],
            )],
        );
        let mut certs = Verifier::new(&ctx).compute(ctx.funcs.iter()).unwrap();
        assert!(certs[0].blocks.len() >= 2);
        certs[0].blocks.remove(0);
        check_error(&ctx, &certs, FuncError::BasicBlockMismatch);
    }

    #[test]
    fn certificate_not_a_fixed_point() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(
                0,
                vec![],
                vec![
                    LoadConst(0),
                    BranchTrue(4),
                    LoadConst(0),
                    Branch(5),
                    LoadConst(0),
                    Ret,
                ],
            )],
        );
        let mut certs = Verifier::new(&ctx).compute(ctx.funcs.iter()).unwrap();
        certs[0].blocks[0].1.operands = usize::MAX;
        let res = Verifier::new(&ctx).check(ctx.funcs.iter().zip(certs.iter()));
        match res {
            Err(Error {
                error: FuncError::Inst(offset, InstError::NotAFixedPoint, _),
                ..
            }) if offset == certs[0].blocks[0].0 => {}
            _ => panic!("unexpected result: {res:?}"),
        }
    }

    #[test]
    fn certificate_operand_depth_mismatch() {
        use Inst::*;

        let ctx = link(
            default_mock(),
            vec![func(0, vec![], vec![LoadConst(0), Ret])],
        );
        let mut certs = Verifier::new(&ctx).compute(ctx.funcs.iter()).unwrap();
        certs[0].max_operand_depth += 1;
        check_error(&ctx, &certs, FuncError::OperandDepthMismatch);
    }
}
