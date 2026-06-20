#![deny(warnings)]

pub mod file;
pub mod varint;
pub mod verify;

#[derive(Debug)]
pub enum EncError {
    Io(io::Error),
}

impl From<io::Error> for EncError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type EncResult<T> = result::Result<T, EncError>;

#[derive(Debug)]
pub enum DecError {
    InvalidOpcode(usize),
    Truncated(usize),
    IntegerOverflow(usize),
    Io(io::Error),
}

impl From<io::Error> for DecError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

type DecResult<T> = result::Result<T, DecError>;

#[derive(Debug)]
pub enum Error {
    FileSizeLimit,
    InvalidHeader,
    TrailingJunk(usize),
    BinTabLimit,
    DebugBinTabLimit,
    InvalidUtf8InDebugBinTab,
    SymTabLimit,
    ConstTabLimit,
    PackTabLimit,
    UnpackTabLimit,
    PackLimit(usize),
    StackSlotLimit(usize),
    CodeLimit(usize),
    CertLimit(usize),
    EmptyCode(usize),
    FuncTabLimit,
    FuncTabEmpty,
    FuncDebugTabWrongSize,
    SourceMapEmpty(usize),
    SourceMapLimit(usize),
    SourceMapOffsetBounds(usize, usize),
    SourceMapLineBounds(usize, usize),
    SourceMapLineDeltaZero(usize, usize),
    InvalidStrInSymTab(usize),
    InvalidStrInConstTab(usize),
    InvalidBinInConstTab(usize),
    InvalidSymInConstTab(usize),
    InvalidSymInPackTab(usize, usize),
    InvalidSymInUnpackTab(usize, usize),
    UnpackLimit(usize),
    InvalidConstInUnpackTab(usize, usize),
    ConstKeyInFunctionParam(usize, usize),
    InvalidUnpackInFuncTab(usize),
    InvalidStrInFuncDebugTab(usize),
    InvalidStrInSourceMap(usize, usize),
    InvalidModuleName,
    Verify(verify::Error),
    Malformed(Box<dyn error::Error>),
    Io(io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Error::*;

        match self {
            FileSizeLimit => write!(f, "bytecode file size limit exceeded"),
            InvalidHeader => write!(f, "invalid file header"),
            TrailingJunk(offset) => write!(f, "trailing junk in bytecode file at offset {offset}"),
            BinTabLimit => write!(f, "binary table exceeds size limit"),
            DebugBinTabLimit => write!(f, "debug binary table exceeds size limit"),
            InvalidUtf8InDebugBinTab => write!(f, "debug binary table contains invalid UTF-8"),
            SymTabLimit => write!(f, "symbol table exceeds size limit"),
            ConstTabLimit => write!(f, "constant table exceeds size limit"),
            PackTabLimit => write!(f, "pack table exceeds size limit"),
            UnpackTabLimit => write!(f, "unpack table exceeds size limit"),
            UnpackLimit(index) => {
                write!(f, "unpack exceeds size limit at unpack table index {index}")
            }
            PackLimit(index) => write!(f, "pack exceeds size limit at pack table index {index}"),
            StackSlotLimit(index) => write!(
                f,
                "stack frame slots exceed size limit at function table index {index}"
            ),
            CodeLimit(index) => {
                write!(f, "code exceeds size limit at function table index {index}")
            }
            CertLimit(index) => {
                write!(
                    f,
                    "certificate exceeds size limit at function table index {index}"
                )
            }
            EmptyCode(index) => write!(f, "code is empty at function table index {index}"),
            FuncTabLimit => write!(f, "function table exceeds size limit"),
            FuncTabEmpty => write!(f, "function table is empty"),
            FuncDebugTabWrongSize => write!(
                f,
                "function debug table is neither empty nor the size of the function table"
            ),
            InvalidStrInSymTab(index) => {
                write!(f, "invalid string in symbol table at index {index}")
            }
            InvalidStrInConstTab(index) => {
                write!(f, "invalid string in constant table at index {index}")
            }
            InvalidBinInConstTab(index) => {
                write!(f, "invalid binary data in constant table at index {index}")
            }
            InvalidSymInConstTab(index) => {
                write!(f, "invalid symbol index in constant table at index {index}")
            }
            InvalidSymInPackTab(index, arg) => {
                write!(
                    f,
                    "invalid symbol in pack table at index {index} argument #{arg}"
                )
            }
            InvalidSymInUnpackTab(index, key) => {
                write!(
                    f,
                    "invalid symbol in unpack table at index {index} key #{key}"
                )
            }
            InvalidConstInUnpackTab(index, key) => {
                write!(
                    f,
                    "invalid constant in unpack table at index {index} key #{key}"
                )
            }
            ConstKeyInFunctionParam(table_entry, key) => {
                write!(
                    f,
                    "constant key in function parameter unpack signature (table entry {table_entry}, key {key})"
                )
            }
            InvalidUnpackInFuncTab(index) => {
                write!(
                    f,
                    "invalid unpack table offset in function table table at index {index}"
                )
            }
            InvalidStrInFuncDebugTab(index) => {
                write!(f, "invalid string in function debug table at index {index}")
            }
            InvalidStrInSourceMap(func, index) => {
                write!(
                    f,
                    "invalid string in source map for function #{func} at index {index}"
                )
            }
            SourceMapEmpty(index) => write!(f, "empty source map for function #{index}"),
            SourceMapLimit(index) => {
                write!(f, "source map size limit exceeded for function #{index}")
            }
            SourceMapOffsetBounds(func, index) => {
                write!(
                    f,
                    "source map offset out of bounds for function #{func} at index {index}"
                )
            }
            SourceMapLineBounds(func, index) => {
                write!(
                    f,
                    "source map line out of bounds for function #{func} at index {index}"
                )
            }
            SourceMapLineDeltaZero(func, index) => {
                write!(
                    f,
                    "redundant source map line for function #{func} at index {index}"
                )
            }
            InvalidModuleName => write!(f, "invalid module name in bytecode"),
            Verify(err) => write!(f, "verification failed: {err}"),
            Malformed(error) => error.fmt(f),
            Io(error) => error.fmt(f),
        }
    }
}

impl error::Error for Error {}

impl From<verify::Error> for Error {
    fn from(value: verify::Error) -> Self {
        Self::Verify(value)
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<postcard::Error> for Error {
    fn from(value: postcard::Error) -> Self {
        Self::Malformed(Box::new(value))
    }
}

pub type Result<T> = result::Result<T, Error>;

use serde::{Deserialize, Serialize};

use std::{
    error,
    fmt::{self, Display, Formatter},
    io,
    marker::PhantomData,
    mem,
    ptr::NonNull,
    result, slice,
};

use varint::{IVar, UVar};

pub(crate) mod limit {
    // Constants
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    const FEW: usize = 1 << 12;
    const SOME: usize = 1 << 16;
    const PLENTY: usize = 1 << 20;
    // Maximum size of function bytecode
    pub(crate) const FUNC_SIZE: usize = 256 * KIB;
    // Maximum size of function certificate
    pub(crate) const CERT_ENTRIES: usize = SOME;
    pub(crate) const FUNC_TAB_ENTRIES: usize = PLENTY;
    // Total frame slots (locals and operands) for a function
    pub(crate) const FUNC_FRAME_SLOTS: usize = SOME;
    pub(crate) const SOURCE_MAP_ENTRIES: usize = SOME;
    // Maximum total upvars, summing over nested records
    pub(crate) const UPVAR_TOTAL: usize = FEW;
    pub(crate) const SYMBOL_TAB_ENTRIES: usize = PLENTY;
    pub(crate) const CONST_TAB_ENTRIES: usize = PLENTY;
    pub(crate) const PACK_TAB_ENTRIES: usize = PLENTY;
    pub(crate) const UNPACK_TAB_ENTRIES: usize = PLENTY;
    pub(crate) const PACK_ENTRIES: usize = FEW;
    pub(crate) const UNPACK_ENTRIES: usize = FEW;
    pub(crate) const STRING_LENGTH: usize = MIB;
    pub(crate) const BIN_TAB_SIZE: usize = 100 * MIB;
    pub(crate) const DEBUG_BIN_TAB_SIZE: usize = 100 * MIB;
    pub(crate) const BYTECODE_FILE_SIZE: usize = 100 * MIB;
}

#[repr(usize)]
pub enum Builtin {
    Import,
    Array,
    Dict,
    Iter,
    ConcatStr,
    ConcatArg,
    Args,
    ClassCreate,
    Guard,
    Throw,
    ConcatBin,
    Range,
    _LEN,
}

pub mod builtin {
    use super::Builtin::*;
    pub const IMPORT: usize = Import as usize;
    pub const ARRAY: usize = Array as usize;
    pub const DICT: usize = Dict as usize;
    pub const RANGE: usize = Range as usize;
    pub const ITER: usize = Iter as usize;
    pub const CONCAT_STR: usize = ConcatStr as usize;
    pub const CONCAT_ARG: usize = ConcatArg as usize;
    pub const ARGS: usize = Args as usize;
    pub const CLASS_CREATE: usize = ClassCreate as usize;
    pub const GUARD: usize = Guard as usize;
    pub const THROW: usize = Throw as usize;
    pub const CONCAT_BIN: usize = ConcatBin as usize;
}

pub const BUILTINS: [&str; Builtin::_LEN as usize] = [
    "import",
    "array",
    "dict",
    "range",
    "iter",
    "concat_str",
    "concat_arg",
    "args",
    "class_create",
    "guard",
    "throw",
    "concat_bin",
];

trait Encode {
    fn encode(&self, w: &mut impl io::Write) -> EncResult<()>;
}

trait Decode: Sized {
    fn decode<R: io::Read + io::Seek>(r: &mut R) -> DecResult<Self>;
}

trait UnsafeDecode: Sized {
    unsafe fn decode(r: &mut NonNull<u8>) -> Self;
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
#[repr(u8)]
pub enum Opcode {
    Add = 1,
    Call,       // sig: uvar
    MethodCall, // sym: uvar, sig: uvar
    Builtin,    // idx: uvar, sig: uvar
    Div,
    Ediv,
    Dup,
    Swap,
    LoadConst, // id: uvar
    LoadLocal, // index: uvar
    LoadUpvar, // index: uvar, depth: uvar
    Get,       // sym: uvar
    Set,       // sym: uvar
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
    Pop,
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
    StoreLocal, // index: uvar
    StoreUpvar, // index: uvar, depth: uvar
    Sub,
    PushUpvars, // count: uvar
    PopUpvars,
    Close, // func: uvar
    Ret,
    Branch,      // offset: ivar
    BranchTrue,  // offset: ivar
    BranchFalse, // offset: ivar
    Reify,       // pack: uvar
    Next,
    Unpack,   // unpack: uvar
    NlGuard,  // func: uvar
    NlBranch, // depth: uvar, indicator: uvar
    Shl,
    Shr,
}

// Decoded instruction
#[derive(Debug, Clone, Copy)]
pub enum Inst {
    Add,
    Call(usize),
    MethodCall(usize, usize),
    Builtin(usize, usize),
    Div,
    Ediv,
    Dup,
    Swap(usize, usize),
    LoadConst(usize),
    LoadLocal(usize),
    LoadUpvar(usize, usize),
    Get(usize),
    Set(usize),
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
    Close(usize),
    Ret,
    Branch(isize),
    BranchTrue(isize),
    BranchFalse(isize),
    Reify(usize),
    Next,
    Unpack(usize),
    NlGuard(usize),
    NlBranch(usize, usize),
    Shl,
    Shr,
}

#[cfg(feature = "debug")]
impl Display for Inst {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Inst::*;

        match self {
            Pop => write!(f, "pop"),
            Dup => write!(f, "dup"),
            Swap(i, j) => write!(f, "swap {i} {j}"),
            Add => write!(f, "add"),
            Sub => write!(f, "sub"),
            Mul => write!(f, "mul"),
            Div => write!(f, "div"),
            Ediv => write!(f, "ediv"),
            Mod => write!(f, "mod"),
            Eq => write!(f, "eq"),
            Ne => write!(f, "ne"),
            Gt => write!(f, "gt"),
            Lt => write!(f, "lt"),
            Gte => write!(f, "gte"),
            Lte => write!(f, "lte"),
            Neg => write!(f, "neg"),
            Not => write!(f, "not"),
            BitNot => write!(f, "bnot"),
            BitAnd => write!(f, "band"),
            BitOr => write!(f, "bor"),
            BitXor => write!(f, "bxor"),
            Shl => write!(f, "shl"),
            Shr => write!(f, "shr"),
            Ret => write!(f, "ret"),
            LoadConst(id) => write!(f, "ldc #{id}"),
            Get(id) => write!(f, "get #{id}"),
            Set(id) => write!(f, "set #{id}"),
            Index => write!(f, "indx"),
            Assign => write!(f, "assn"),
            Call(id) => write!(f, "call #{id}"),
            MethodCall(sym, sig) => write!(f, "mcll #{sym} #{sig}"),
            Builtin(idx, sig) => write!(
                f,
                "bltn #{idx}({}) #{sig}",
                BUILTINS.get(*idx).unwrap_or(&"INVALID")
            ),
            LoadLocal(idx) => write!(f, "ldl #{idx}"),
            StoreLocal(idx) => write!(f, "stl #{idx}"),
            LoadUpvar(idx, depth) => write!(f, "ldu #{idx},{depth}"),
            StoreUpvar(idx, depth) => write!(f, "stu #{idx},{depth}"),
            PushUpvars(count) => write!(f, "pshu #{count}"),
            PopUpvars => write!(f, "popu"),
            Close(id) => write!(f, "cls #{id}"),
            Branch(ofs) => write!(f, "br {ofs}"),
            BranchTrue(ofs) => write!(f, "brt {ofs}"),
            BranchFalse(ofs) => write!(f, "brf {ofs}"),
            Reify(id) => write!(f, "rfy #{id}"),
            Next => write!(f, "next"),
            Unpack(id) => write!(f, "unpk #{id}"),
            NlGuard(id) => write!(f, "nlgd #{id}"),
            NlBranch(depth, indicator) => write!(f, "nlbr {depth},{indicator}"),
        }
    }
}

impl Encode for Opcode {
    fn encode(&self, w: &mut impl io::Write) -> EncResult<()> {
        let val = *self as u8;
        w.write_all(slice::from_ref(&val))?;
        Ok(())
    }
}

impl Decode for Opcode {
    fn decode<R: io::Read + io::Seek>(r: &mut R) -> DecResult<Self> {
        use Opcode::*;

        const TABLE: [Option<Opcode>; 256] = const {
            let table = [
                None,
                Some(Add),
                Some(Call),
                Some(MethodCall),
                Some(Builtin),
                Some(Div),
                Some(Ediv),
                Some(Dup),
                Some(Swap),
                Some(LoadConst),
                Some(LoadLocal),
                Some(LoadUpvar),
                Some(Get),
                Some(Set),
                Some(Index),
                Some(Assign),
                Some(Mod),
                Some(Mul),
                Some(Neg),
                Some(Not),
                Some(BitNot),
                Some(BitAnd),
                Some(BitOr),
                Some(BitXor),
                Some(Pop),
                Some(Eq),
                Some(Ne),
                Some(Gt),
                Some(Lt),
                Some(Gte),
                Some(Lte),
                Some(StoreLocal),
                Some(StoreUpvar),
                Some(Sub),
                Some(PushUpvars),
                Some(PopUpvars),
                Some(Close),
                Some(Ret),
                Some(Branch),
                Some(BranchTrue),
                Some(BranchFalse),
                Some(Reify),
                Some(Next),
                Some(Unpack),
                Some(NlGuard),
                Some(NlBranch),
                Some(Shl),
                Some(Shr),
                // This is worse than anything
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ];
            if size_of_val(&table) != 256 {
                panic!("reality is broken")
            }
            table
        };

        let mut val = 0;

        let offset = r.stream_position()?.try_into().unwrap();
        r.read_exact(slice::from_mut(&mut val))?;
        TABLE[val as usize].ok_or(DecError::InvalidOpcode(offset))
    }
}

impl Inst {
    pub fn write<W: io::Write>(&self, w: &mut InstEncoder<W>) -> EncResult<()> {
        use Inst::*;

        match self {
            Add => w.opcode(Opcode::Add),
            Div => w.opcode(Opcode::Div),
            Ediv => w.opcode(Opcode::Ediv),
            Mod => w.opcode(Opcode::Mod),
            Mul => w.opcode(Opcode::Mul),
            Neg => w.opcode(Opcode::Neg),
            Not => w.opcode(Opcode::Not),
            BitNot => w.opcode(Opcode::BitNot),
            BitAnd => w.opcode(Opcode::BitAnd),
            BitOr => w.opcode(Opcode::BitOr),
            BitXor => w.opcode(Opcode::BitXor),
            Pop => w.opcode(Opcode::Pop),
            Eq => w.opcode(Opcode::Eq),
            Ne => w.opcode(Opcode::Ne),
            Gt => w.opcode(Opcode::Gt),
            Lt => w.opcode(Opcode::Lt),
            Gte => w.opcode(Opcode::Gte),
            Lte => w.opcode(Opcode::Lte),
            Dup => w.opcode(Opcode::Dup),
            Swap(i, j) => {
                w.opcode(Opcode::Swap)?;
                w.usize(*i)?;
                w.usize(*j)
            }
            Sub => w.opcode(Opcode::Sub),
            PopUpvars => w.opcode(Opcode::PopUpvars),
            Ret => w.opcode(Opcode::Ret),
            Call(id) => {
                w.opcode(Opcode::Call)?;
                w.usize(*id)
            }
            MethodCall(sym, sig) => {
                w.opcode(Opcode::MethodCall)?;
                w.usize(*sym)?;
                w.usize(*sig)
            }
            Builtin(idx, sig) => {
                w.opcode(Opcode::Builtin)?;
                w.usize(*idx)?;
                w.usize(*sig)
            }
            LoadConst(id) => {
                w.opcode(Opcode::LoadConst)?;
                w.usize(*id)
            }
            LoadLocal(idx) => {
                w.opcode(Opcode::LoadLocal)?;
                w.usize(*idx)
            }
            LoadUpvar(idx, depth) => {
                w.opcode(Opcode::LoadUpvar)?;
                w.usize(*idx)?;
                w.usize(*depth)
            }
            StoreLocal(id) => {
                w.opcode(Opcode::StoreLocal)?;
                w.usize(*id)
            }
            StoreUpvar(idx, depth) => {
                w.opcode(Opcode::StoreUpvar)?;
                w.usize(*idx)?;
                w.usize(*depth)
            }
            Get(id) => {
                w.opcode(Opcode::Get)?;
                w.usize(*id)
            }
            Set(id) => {
                w.opcode(Opcode::Set)?;
                w.usize(*id)
            }
            Index => w.opcode(Opcode::Index),
            Assign => w.opcode(Opcode::Assign),
            PushUpvars(count) => {
                w.opcode(Opcode::PushUpvars)?;
                w.usize(*count)
            }
            Close(id) => {
                w.opcode(Opcode::Close)?;
                w.usize(*id)
            }
            Branch(target) => {
                w.opcode(Opcode::Branch)?;
                w.isize(*target)
            }
            BranchTrue(target) => {
                w.opcode(Opcode::BranchTrue)?;
                w.isize(*target)
            }
            BranchFalse(target) => {
                w.opcode(Opcode::BranchFalse)?;
                w.isize(*target)
            }
            Reify(id) => {
                w.opcode(Opcode::Reify)?;
                w.usize(*id)
            }
            Next => w.opcode(Opcode::Next),
            Unpack(id) => {
                w.opcode(Opcode::Unpack)?;
                w.usize(*id)
            }
            NlGuard(id) => {
                w.opcode(Opcode::NlGuard)?;
                w.usize(*id)
            }
            NlBranch(depth, indicator) => {
                w.opcode(Opcode::NlBranch)?;
                w.usize(*depth)?;
                w.usize(*indicator)
            }
            Shl => w.opcode(Opcode::Shl),
            Shr => w.opcode(Opcode::Shr),
        }
    }
}

impl Encode for Inst {
    fn encode(&self, w: &mut impl io::Write) -> EncResult<()> {
        let mut write = InstEncoder::new(w);
        self.write(&mut write)
    }
}

pub struct InstEncoder<W: io::Write>(W);

impl<W: io::Write> InstEncoder<W> {
    pub fn new(write: W) -> Self {
        Self(write)
    }

    pub fn opcode(&mut self, op: Opcode) -> EncResult<()> {
        op.encode(&mut self.0)
    }

    pub fn uvar(&mut self, val: UVar) -> EncResult<()> {
        val.encode(&mut self.0)
    }

    pub fn ivar(&mut self, val: IVar) -> EncResult<()> {
        val.encode(&mut self.0)
    }

    pub fn usize(&mut self, val: usize) -> EncResult<()> {
        self.uvar(val.try_into().expect("uvar not large enough for usize?!"))
    }

    pub fn isize(&mut self, val: isize) -> EncResult<()> {
        self.ivar(val.try_into().expect("ivar not large enough for isize?!"))
    }
}

impl<W: io::Write + io::Seek> InstEncoder<W> {
    pub fn offset(&mut self) -> io::Result<usize> {
        self.0
            .stream_position()?
            .try_into()
            .map_err(|_| io::Error::other("instruction stream too long"))
    }
}

pub struct InstDecoder<R: io::Read>(R);

impl<R: io::Read + io::Seek> InstDecoder<R> {
    pub fn new(read: R) -> Self {
        Self(read)
    }

    pub fn opcode(&mut self) -> DecResult<Opcode> {
        Decode::decode(&mut self.0)
    }

    pub fn uvar(&mut self) -> DecResult<UVar> {
        let offset = self.offset()?;
        Decode::decode(&mut self.0).map_err(|e| match e {
            DecError::Io(e) => match e.kind() {
                io::ErrorKind::UnexpectedEof => DecError::Truncated(offset),
                _ => DecError::Io(e),
            },
            e => e,
        })
    }

    pub fn ivar(&mut self) -> DecResult<IVar> {
        let offset = self.offset()?;
        Decode::decode(&mut self.0).map_err(|e| match e {
            DecError::Io(e) => match e.kind() {
                io::ErrorKind::UnexpectedEof => DecError::Truncated(offset),
                _ => DecError::Io(e),
            },
            e => e,
        })
    }

    pub fn usize(&mut self) -> DecResult<usize> {
        let offset = self.offset()?;
        self.uvar()
            .and_then(|v| v.try_into().map_err(|_| DecError::IntegerOverflow(offset)))
    }

    pub fn isize(&mut self) -> DecResult<isize> {
        let offset = self.offset()?;
        self.ivar()
            .and_then(|v| v.try_into().map_err(|_| DecError::IntegerOverflow(offset)))
    }

    pub fn offset(&mut self) -> io::Result<usize> {
        self.0
            .stream_position()?
            .try_into()
            .map_err(|_| io::Error::other("instruction stream too long"))
    }

    pub fn with_offsets(self) -> WithOffsets<R> {
        WithOffsets(self)
    }
}

impl<R: io::Read + io::Seek> Iterator for InstDecoder<R> {
    type Item = DecResult<Inst>;

    fn next(&mut self) -> Option<Self::Item> {
        use Opcode::*;

        let op = match self.opcode() {
            Ok(op) => op,
            Err(DecError::Io(e)) => {
                match e.kind() {
                    // End of instruction stream
                    io::ErrorKind::UnexpectedEof => return None,
                    _ => return Some(Err(DecError::Io(e))),
                }
            }
            Err(err) => return Some(Err(err)),
        };
        Some((|| {
            Ok(match op {
                Add => Inst::Add,
                Call => Inst::Call(self.usize()?),
                MethodCall => Inst::MethodCall(self.usize()?, self.usize()?),
                Builtin => Inst::Builtin(self.usize()?, self.usize()?),
                Div => Inst::Div,
                Ediv => Inst::Ediv,
                Dup => Inst::Dup,
                Swap => Inst::Swap(self.usize()?, self.usize()?),
                LoadConst => Inst::LoadConst(self.usize()?),
                LoadLocal => Inst::LoadLocal(self.usize()?),
                LoadUpvar => Inst::LoadUpvar(self.usize()?, self.usize()?),
                Get => Inst::Get(self.usize()?),
                Set => Inst::Set(self.usize()?),
                Index => Inst::Index,
                Assign => Inst::Assign,
                Mod => Inst::Mod,
                Mul => Inst::Mul,
                Neg => Inst::Neg,
                Not => Inst::Not,
                BitNot => Inst::BitNot,
                BitOr => Inst::BitOr,
                BitAnd => Inst::BitAnd,
                BitXor => Inst::BitXor,
                Pop => Inst::Pop,
                Eq => Inst::Eq,
                Ne => Inst::Ne,
                Gt => Inst::Gt,
                Lt => Inst::Lt,
                Gte => Inst::Gte,
                Lte => Inst::Lte,
                StoreLocal => Inst::StoreLocal(self.usize()?),
                StoreUpvar => Inst::StoreUpvar(self.usize()?, self.usize()?),
                Sub => Inst::Sub,
                PushUpvars => Inst::PushUpvars(self.usize()?),
                PopUpvars => Inst::PopUpvars,
                Close => Inst::Close(self.usize()?),
                Ret => Inst::Ret,
                Branch => Inst::Branch(self.isize()?),
                BranchTrue => Inst::BranchTrue(self.isize()?),
                BranchFalse => Inst::BranchFalse(self.isize()?),
                Reify => Inst::Reify(self.usize()?),
                Next => Inst::Next,
                Unpack => Inst::Unpack(self.usize()?),
                NlGuard => Inst::NlGuard(self.usize()?),
                NlBranch => Inst::NlBranch(self.usize()?, self.usize()?),
                Shl => Inst::Shl,
                Shr => Inst::Shr,
            })
        })())
    }
}

pub struct InstOffsets {
    pub before: usize,
    pub inst: Inst,
    pub after: usize,
}

pub struct WithOffsets<R: io::Read + io::Seek>(InstDecoder<R>);

impl<R: io::Read + io::Seek> Iterator for WithOffsets<R> {
    type Item = DecResult<InstOffsets>;

    fn next(&mut self) -> Option<Self::Item> {
        let pre = match self.0.offset() {
            Ok(offset) => offset,
            Err(err) => return Some(Err(err.into())),
        };
        let next = self.0.next();
        let post = match self.0.offset() {
            Ok(offset) => offset,
            Err(err) => return Some(Err(err.into())),
        };
        next.map(|item| {
            item.map(|inst| InstOffsets {
                before: pre,
                inst,
                after: post,
            })
        })
    }
}

/// All of the unsafe methods of this type depend on the bytecode being
/// previously validated and instructions being processed correctly.
/// It should not be used outside of the bytecode interpreter.
pub struct UnsafeInstDecoder<'a> {
    start: NonNull<u8>,
    pc: NonNull<u8>,
    phantom: PhantomData<&'a [u8]>,
}

impl<'a> UnsafeInstDecoder<'a> {
    pub fn new(bytecode: &'a [u8]) -> Self {
        let pc = unsafe { NonNull::new_unchecked(bytecode.as_ptr() as *mut u8) };
        Self {
            start: pc,
            pc,
            phantom: PhantomData,
        }
    }

    /// # Safety
    /// Current offset must be in bounds and at valid opcode
    pub unsafe fn opcode(&mut self) -> Opcode {
        unsafe {
            let op = *self.pc.as_ptr();
            self.pc = self.pc.add(1);
            mem::transmute::<u8, Opcode>(op)
        }
    }

    /// # Safety
    /// Current offset must at valid unsigned integer within bounds
    pub unsafe fn uvar(&mut self) -> UVar {
        unsafe { UnsafeDecode::decode(&mut self.pc) }
    }

    /// # Safety
    /// Current offset must at valid signed integer within bounds
    pub unsafe fn ivar(&mut self) -> IVar {
        unsafe { UnsafeDecode::decode(&mut self.pc) }
    }

    /// # Safety
    /// Current offset must at valid unsigned integer within bounds that fits in usize
    pub unsafe fn usize(&mut self) -> usize {
        unsafe { self.uvar() as usize }
    }

    /// # Safety
    /// Current offset must at valid signed integer within bounds that fits in isize
    pub unsafe fn isize(&mut self) -> isize {
        unsafe { self.ivar() as isize }
    }

    /// # Safety
    /// Target offset must be the start of an in-bounds instruction
    pub unsafe fn seek(&mut self, offset: isize) {
        unsafe { self.pc = self.pc.offset(offset) }
    }

    pub fn offset(&self) -> usize {
        self.pc.addr().get() - self.start.addr().get()
    }
}

pub trait Phase: Sized {
    type Bytes;
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub enum Arg {
    Value,
    Pack,
    Key(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Variadic {
    /// No rest parameter - strict validation, no extra args allowed
    None,
    /// Rest parameter without capture (...) - allow extra args but don't build iterator
    Discard,
    /// Rest parameter with capture (...name) - build iterator for remaining items
    Capture,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Func<P: Phase> {
    // Index in unpack table for function parameters
    pub sig: usize,
    pub locals: usize,
    pub upvars: Vec<usize>,
    pub bytecode: P::Bytes,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub(crate) enum LocalState {
    Invalid,
    Value,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BlockState {
    operands: usize,
    locals: Vec<LocalState>,
    upvars: Vec<usize>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
pub struct Certificate {
    pub max_operand_depth: usize,
    pub(crate) blocks: Vec<(usize, BlockState)>,
}
