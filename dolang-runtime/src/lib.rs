#![deny(warnings)]

/// Writes formatted data to a [`Format`](crate::value::Format) destination.
#[macro_export]
macro_rules! fmt {
    ($strand:expr, $destination:expr, $($arg:tt)*) => {
        $crate::value::Format::write_fmt(
            $destination,
            $strand,
            format_args!($($arg)*),
        )
    };
}

pub mod arg;
pub mod error;
pub mod frame;
pub(crate) mod gc;
pub(crate) mod interp;
pub mod object;
pub(crate) mod sig;
pub(crate) mod stdlib;
pub mod strand;
pub mod sym;
pub mod value;
pub mod vm;

use std::{
    borrow::Cow,
    ops::{ControlFlow, Range},
};

use dolang_bytecode::{self as bytecode, Phase};
use dolang_util::alias;

use crate::{
    object::{protocol::GcObj, sym::SymObj},
    sym::Sym,
    value::Value,
};

/// How often (in iterations) to check for interrupts in inner loops.
pub(crate) const INTERRUPT_INTERVAL: usize = 1024;

pub(crate) struct Runtime;

impl Phase for Runtime {
    type Bytes = Range<usize>;
}

pub(crate) type Func = bytecode::Func<Runtime>;

pub(crate) struct FuncDebug {
    pub(crate) name: Range<usize>,
    pub(crate) sourcemap: alias::Box<[(usize, u32, Range<usize>)]>,
}

// Loaded bytecode program
pub(crate) struct Program<'v> {
    pub(crate) bytecode: Cow<'static, [u8]>,
    pub(crate) funcs: alias::Box<[(Func, usize)]>,
    pub(crate) symtab: Vec<Sym<'v, 'v>>,
    pub(crate) consttab: alias::Box<[Value<'v>]>,
    pub(crate) packtab: alias::Box<[sig::Pack<'v, 'v>]>,
    pub(crate) unpacktab: alias::Box<[sig::Unpack<'v, 'v>]>,
    pub(crate) debugbintab: Range<usize>,
    pub(crate) funcdebugs: alias::Box<[FuncDebug]>,
    pub(crate) module_name: Option<Range<usize>>,
    /// Safety: this roots symbols used by above fields, which are thus implicitly self-referential
    #[allow(dead_code)]
    pub(crate) symroots: Vec<GcObj<'v, SymObj>>,
    pub(crate) id: u32,
}

impl<'v> Program<'v> {
    /// Returns the debug string table as `&str` directly from the owned bytecode.
    ///
    /// Sound because `verify_debugbintab` validates the entire table as UTF-8
    /// before a `Program` can be constructed.
    pub(crate) fn debug_strtab(&self) -> &str {
        // SAFETY: the bytecode verifier rejects any file whose debugbintab
        // contains invalid UTF-8, so this range is guaranteed to be valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.bytecode[self.debugbintab.clone()]) }
    }
}

unsafe impl<'v> gc::Collect for Program<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn gc::arena::Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}
