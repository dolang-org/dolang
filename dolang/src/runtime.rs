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
        AsBoundFunction, AsFunction, AsSym, AsTuple, BinEmbryo, Empty, Input, Nil, Output, Root,
        Singleton, Slot, StrEmbryo, TypeObject, Value, fmt,
        view::{
            Array, Bin, Dict, DictPairs, Fmt, FmtParam, FmtValue, ObjectId, ObjectView, PinBin,
            PinStr, Record, RecordPairs, Set, SetMembers, Str, Struct, StructPairs, Tuple, View,
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
        Cast, Instance, Mut, Object, Ref, Rest, Spread, SpreadContext, Type, TypeBuilder, TypeMut,
        TypeRef, Unpack, UnpackItem,
    };
    pub use dolang_runtime::object::{
        array_view::{ArrayLike, ArrayView},
        dict_view::{DictLike, DictView, DictViewSink},
        flags::{FlagLike, FlagLikeExt, Flags, FlagsInstanceExt, FlagsTypeExt},
        interface::{InterfaceTag, Request},
    };
}

pub use object::{Instance, Object, Type};

/// Native byte I/O handles
pub mod io {
    pub use dolang_runtime::io::{
        ByteHandle, ByteIo, IntoByteIo, IoCaps, Read, ReadSeek, ReadWrite, ReadWriteSeek, Write,
        WriteSeek, buffered, error, take,
    };
}

/// VM management
pub mod vm {
    pub use dolang_runtime::frame::Frame;
    pub use dolang_runtime::vm::{
        Alloc, AllocExt, Builder, Bytecode, ModuleBuilder, Register, State, StateTag, Vm,
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
