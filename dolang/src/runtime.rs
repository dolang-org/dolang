pub use dolang_runtime::{
    arg::{Arg, Args},
    call, method,
    sym::Sym,
    unpack,
};

/// Value manipulation
pub mod value {
    pub use dolang_runtime::value::{
        Empty, Input, Nil, Output, Root, Singleton, Slot, TypeObject, Value,
        view::{
            Array, Bin, Dict, DictPairs, ObjectId, ObjectView, PinBin, PinStr, Record, RecordPairs,
            Str, Tuple, View,
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
    pub use dolang_runtime::object::native::{
        Instance, Mut, Object, Ref, Spread, SpreadContext, Type, TypeBuilder, TypeMut, TypeRef,
        Unpack, UnpackItem,
    };
}

pub use object::{Instance, Object, Type};

/// VM management
pub mod vm {
    pub use dolang_runtime::frame::Frame;
    pub use dolang_runtime::vm::{Alloc, Builder, Bytecode, ModuleBuilder, State, Stateful, Vm};
}

pub use vm::{Alloc, Bytecode, Frame, State, Vm};

/// Strands
pub mod strand {
    pub use dolang_runtime::strand::{
        InterruptToken, Local, LocalKey, LocalRootKey, Redirect, Strand,
    };
}

pub use strand::Strand;
