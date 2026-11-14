pub use dolang_runtime::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::{Arg, Args},
    call, method, redirect,
    sym::Sym,
    unpack, with_instance, with_interrupt_mask, with_interrupt_token, with_slice, with_slots,
    with_str,
};

/// Value manipulation
pub mod value {
    pub use dolang_runtime::value::{
        BinEmbryo, Input, Nil, Output, Root, Singleton, Slot, StrEmbryo, TypeObject, Value, fmt,
        view::{
            Array, Bin, Dict, DictPairs, Fmt, FmtParam, FmtValue, ObjectId, ObjectView, Pack,
            PackPairs, PinBin, PinStr, Record, RecordPairs, Set, SetMembers, Str, Struct,
            StructPairs, Tuple, View,
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
    pub use dolang_runtime::{
        fmt,
        object::{
            array_view::{ArrayLike, ArrayView},
            dict_view::{DictLike, DictView, DictViewSink},
            flags::{FlagCoercion, FlagLike, FlagLikeExt, Flags, FlagsInstanceExt, FlagsTypeExt},
            interface::{InterfaceTag, Request},
            native::{
                Cast, Instance, Mut, Object, Ref, Rest, Spread, SpreadContext, Type, TypeBuilder,
                TypeMut, TypeRef, Unpack, UnpackItem,
            },
        },
        value::fmt::Format,
    };
}

pub use object::{Instance, Object, Type};

/// Native byte I/O handles
pub mod io {
    pub use dolang_runtime::io::{
        AsyncFork, ByteHandle, ByteIo, IntoByteIo, IntoIoError, IoCaps, IoError, IoErrorExt,
        IoResultExt, Read, ReadSeek, ReadWrite, ReadWriteSeek, Shared, Write, WriteSeek, concat,
        error, get,
    };
}

/// VM management
pub mod vm {
    pub use dolang_runtime::frame::{Frame, Pos, Span};
    pub use dolang_runtime::vm::{
        Alloc, AllocExt, Builder, Bytecode, BytecodeError, ModuleBuilder, Register, State,
        StateTag, Vm,
    };
}

pub use vm::{Alloc, AllocExt, Bytecode, BytecodeError, Frame, State, Vm};

/// Strands
pub mod strand {
    pub use dolang_runtime::strand::{
        InheritKind, InterruptMask, InterruptToken, Local, LocalKey, LocalRootKey, Redirect, Strand,
    };
}

pub use strand::Strand;
