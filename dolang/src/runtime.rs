pub use dolang_runtime::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::{Arg, Args},
    call, method,
    sym::Sym,
    unpack,
};

/// Value manipulation
pub mod value {
    pub use dolang_runtime::value::{
        AsBoundFunction, AsFunction, AsTuple, BinEmbryo, Empty, Input, Nil, Output, Root,
        Singleton, Slot, StrEmbryo, TypeObject, Value, fmt,
        view::{
            Array, Bin, Dict, DictPairs, Fmt, FmtParam, FmtValue, ObjectId, ObjectView, PinBin,
            PinStr, Record, RecordPairs, Set, SetMembers, Str, Tuple, View,
        },
    };
}

pub use value::{Input, Output, Slot, Value};

/// Error handling
pub mod error {
    pub use dolang_runtime::error::{
        BacktraceEntry, BacktraceIter, Error, ErrorExt, ErrorKind, Result, ResultExt,
    };
}

pub use error::{Error, Result};

/// Native objects
pub mod object {
    pub use dolang_runtime::fmt;
    pub use dolang_runtime::object::native::{
        Cast, Instance, Mut, Object, Ref, Spread, SpreadContext, Type, TypeBuilder, TypeMut,
        TypeRef, Unpack, UnpackItem,
    };
    pub use dolang_runtime::object::{
        array_view::{ArrayLike, ArrayView},
        dict_view::{DictLike, DictView, DictViewSink},
        flags::{FlagLike, FlagLikeExt, Flags, FlagsInstanceExt, FlagsTypeExt},
    };
}

pub use object::{Instance, Object, Type};

/// VM management
pub mod vm {
    pub use dolang_runtime::frame::Frame;
    pub use dolang_runtime::vm::{
        Alloc, AllocExt, Builder, Bytecode, ModuleBuilder, Register, State, Stateful, Vm,
    };
}

pub use vm::{Alloc, AllocExt, Bytecode, Frame, State, Vm};

/// Strands
pub mod strand {
    pub use dolang_runtime::strand::{
        InheritKind, InterruptMask, InterruptToken, Local, LocalKey, LocalRootKey, Redirect, Strand,
    };
}

pub use strand::Strand;
