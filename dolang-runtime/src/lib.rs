#![deny(warnings)]

/// Writes formatted data to a [`Format`](crate::value::fmt::Format) destination.
#[macro_export]
macro_rules! fmt {
    ($strand:expr, $destination:expr, $($arg:tt)*) => {
        $crate::value::fmt::Format::write_fmt(
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
pub mod io;
pub mod object;
pub(crate) mod sig;
pub(crate) mod stdlib;
pub mod strand;
pub mod sym;
#[cfg(test)]
pub(crate) mod test_support;
pub mod value;
pub mod vm;

use std::{
    borrow::Cow,
    ops::{ControlFlow, Range},
};

use crate::{
    object::{protocol::GcObj, sym::SymObj},
    sym::Sym,
    value::Value,
};
use dolang_bytecode as bytecode;
use dolang_util::alias;

/// How often (in iterations) to check for interrupts in inner loops.
pub(crate) const INTERRUPT_INTERVAL: usize = 1024;

/// Default item size for iterators which produce a stream of bytes.
///
/// This is large enough to amortize file and remote-filesystem operations while
/// still applying backpressure between individual [`Iter`](crate::value::TypeObject::Iter)
/// and [`Sink`](crate::value::TypeObject::Sink) operations.
pub const BYTE_STREAM_CHUNK_SIZE: usize = 512 * 1024;

/// A loaded function; its bytecode range is absolute within the program's bytecode
pub(crate) type Func = bytecode::Func;

pub(crate) struct FuncDebug {
    pub(crate) name: Range<usize>,
    pub(crate) file: Range<usize>,
    pub(crate) sourcemap: alias::Box<[(usize, u32)]>,
}

// Loaded bytecode program
pub(crate) struct Program<'v> {
    pub(crate) importer: Value<'v>,
}

pub(crate) struct ProgramAnnex<'v> {
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
}

impl<'v> ProgramAnnex<'v> {
    /// Returns the debug string table as `&str` directly from the owned bytecode.
    ///
    /// Sound because `verify_debug` validates the entire region as UTF-8
    /// before a `Program` can be constructed.
    pub(crate) fn debug_strtab(&self) -> &str {
        // SAFETY: the bytecode verifier rejects any file whose debug region
        // contains invalid UTF-8, so this range is guaranteed to be valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.bytecode[self.debugbintab.clone()]) }
    }
}

unsafe impl<'v> gc::Collect for Program<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ProgramAnnex<'v>;

    fn accept(&self, visit: &mut dyn gc::arena::Visit) -> ControlFlow<()> {
        self.importer.accept(visit)
    }

    fn clear(&mut self) {
        self.importer = Value::NIL;
    }
}

impl gc::Annex for ProgramAnnex<'_> {
    fn accept(&self, _visit: &mut dyn gc::arena::Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}
