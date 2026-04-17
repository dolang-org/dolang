use std::{
    borrow::Cow,
    error,
    fmt::{self, Display, Formatter},
    ops::ControlFlow,
};

use dolang_util::alias;

use crate::{
    arg::Args,
    error::{Error, ErrorKind, Result, ResultExt},
    gc::{Collect, arena::Visit},
    strand::Strand,
    unpack,
    value::{Slot, Value},
    vm::Vm,
};

use super::protocol::{Inspect, Protocol, Recv};

#[derive(Debug)]
pub(crate) enum Boxed<'v> {
    Unsupported,
    Immutable,
    Concurrency(Option<Cow<'v, str>>),
    Type(Cow<'v, str>),
    Value(Cow<'v, str>),
    State(Cow<'v, str>),
    Overflow,
    ZeroDiv,
    SinkStop,
    IterStop,
    Index,
    Canceled,
    Field(alias::Box<str>),
    UnexpectedPos(usize),
    UnexpectedKey(alias::Box<str>),
    MissingPos(usize),
    MissingKey(alias::Box<str>),
    CyclicImport(alias::Box<str>),
    Import(alias::Box<str>),
    Compile(Box<dyn error::Error>),
    Bytecode(Box<dyn error::Error>),
    Runtime(Box<dyn error::Error>),
    Interrupt(Box<dyn error::Error>),
}

impl<'v> Boxed<'v> {
    pub(crate) fn kind(&self) -> ErrorKind {
        use Boxed::*;
        match self {
            Unsupported => ErrorKind::Unsupported,
            Immutable => ErrorKind::Immutable,
            Concurrency(_) => ErrorKind::Concurrency,
            Type(_) => ErrorKind::Type,
            Value(_) => ErrorKind::Value,
            State(_) => ErrorKind::State,
            Index => ErrorKind::Index,
            Field(_) => ErrorKind::Field,
            UnexpectedPos(_) => ErrorKind::UnexpectedPos,
            UnexpectedKey(_) => ErrorKind::UnexpectedKey,
            MissingPos(_) => ErrorKind::MissingPos,
            MissingKey(_) => ErrorKind::MissingKey,
            Overflow => ErrorKind::Overflow,
            ZeroDiv => ErrorKind::ZeroDiv,
            SinkStop => ErrorKind::SinkStop,
            IterStop => ErrorKind::IterStop,
            CyclicImport(_) => ErrorKind::CyclicImport,
            Import(_) => ErrorKind::Import,
            Compile(_) => ErrorKind::Compile,
            Bytecode(_) => ErrorKind::Bytecode,
            Runtime(_) => ErrorKind::Runtime,
            Interrupt(_) => ErrorKind::Interrupt,
            Canceled => ErrorKind::Canceled,
        }
    }
}

impl<'v> Display for Boxed<'v> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use Boxed::*;

        match self {
            Unsupported => write!(f, "unsupported operation"),
            Immutable => write!(f, "object is immutable"),
            Concurrency(None) => write!(f, "conflicting concurrent operation"),
            Concurrency(Some(msg)) => write!(f, "conflicting concurrent operation: {msg}"),
            Type(msg) => write!(f, "type error: {msg}"),
            Value(msg) => write!(f, "value error: {msg}"),
            State(msg) => write!(f, "state error: {msg}"),
            Overflow => write!(f, "numeric overflow"),
            ZeroDiv => write!(f, "integer zero divisor"),
            SinkStop => write!(f, "output iterator stopped"),
            IterStop => write!(f, "input iterator stopped"),
            Index => write!(f, "index out of range or invalid"),
            Canceled => write!(f, "strand canceled"),
            Field(name) => write!(f, "no such field: {name}"),
            UnexpectedPos(i) => write!(f, "unexpected positional item: {i}"),
            UnexpectedKey(name) => write!(f, "unexpected key item: {name}"),
            MissingPos(i) => write!(f, "missing positional item: {i}"),
            MissingKey(name) => write!(f, "missing key item: {name}"),
            Import(name) => write!(f, "module not found: {name}"),
            CyclicImport(name) => {
                write!(f, "cycle detected importing module: {name}")
            }
            Compile(error) => write!(f, "compile error: {error}"),
            Bytecode(error) => write!(f, "bytecode error: {error}"),
            Runtime(error) => Display::fmt(error, f),
            Interrupt(error) => Display::fmt(error, f),
        }
    }
}

unsafe impl<'v> Collect for Boxed<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for Boxed<'v> {
    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        let class = strand.singletons();
        let err = this.get().kind();
        out.store(match err {
            ErrorKind::Unsupported => class.error_unsupported.dup(),
            ErrorKind::Immutable => class.error_immutable.dup(),
            ErrorKind::Concurrency => class.error_concurrency.dup(),
            ErrorKind::Type => class.error_type.dup(),
            ErrorKind::Value => class.error_value.dup(),
            ErrorKind::State => class.error_state.dup(),
            ErrorKind::Index => class.error_index.dup(),
            ErrorKind::Field => class.error_field.dup(),
            ErrorKind::UnexpectedPos => class.error_unexpected_pos.dup(),
            ErrorKind::UnexpectedKey => class.error_unexpected_key.dup(),
            ErrorKind::MissingPos => class.error_missing_pos.dup(),
            ErrorKind::MissingKey => class.error_missing_key.dup(),
            ErrorKind::Overflow => class.error_overflow.dup(),
            ErrorKind::ZeroDiv => class.error_zerodiv.dup(),
            ErrorKind::SinkStop => class.error_sink_stop.dup(),
            ErrorKind::IterStop => class.error_iter_stop.dup(),
            ErrorKind::CyclicImport => class.error_cyclic_import.dup(),
            ErrorKind::Import => class.error_import.dup(),
            ErrorKind::Compile => class.error_compile.dup(),
            ErrorKind::Bytecode => class.error_bytecode.dup(),
            ErrorKind::Runtime => class.error_runtime.dup(),
            ErrorKind::Interrupt => class.error_interrupt.dup(),
            ErrorKind::Canceled => class.error_canceled.dup(),
        })
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.get()).into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<error: {}>", this.get()).into_do(strand)
    }
}

// ── Error Class ─────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type Error>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: Vec::new(),
        })
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        Err(Error::from_value(strand, value))
    }
}

// ── Error Variant Classes ───────────────────────────────────────

pub(crate) struct VariantType(pub(crate) ErrorKind);

unsafe impl Collect for VariantType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for VariantType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &crate::value::Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || strand.vm().singletons().error.eq(strand, supertype)
            || (is_runtime_superkind(this.get().0)
                && strand.vm().singletons().error_runtime.eq(strand, supertype))
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.error.{}>", variant_name(this.get().0)).into_do(strand)
    }

    fn op_inspect<'a>(this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: this.get().0 == ErrorKind::Runtime,
            members: Vec::new(),
        })
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let kind = this.get().0;
        let mut error = match kind {
            ErrorKind::Unsupported => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::not_supported(strand)
            }
            ErrorKind::Immutable => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::immutable(strand)
            }
            ErrorKind::Concurrency => {
                let ([], [msg]) = unpack!(strand, args, 0, 1)?;
                match msg {
                    Some(msg) => {
                        let msg = expect_string_like(strand, &msg)?;
                        Error::concurrency_msg(strand, msg)
                    }
                    None => Error::concurrency(strand),
                }
            }
            ErrorKind::Type => {
                let ([msg], []) = unpack!(strand, args, 1, 0)?;
                let msg = expect_string_like(strand, &msg)?;
                Error::type_error(strand, msg)
            }
            ErrorKind::Value => {
                let ([msg], []) = unpack!(strand, args, 1, 0)?;
                let msg = expect_string_like(strand, &msg)?;
                Error::value(strand, msg)
            }
            ErrorKind::State => {
                let ([msg], []) = unpack!(strand, args, 1, 0)?;
                let msg = expect_string_like(strand, &msg)?;
                Error::state_error(strand, msg)
            }
            ErrorKind::Index => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::index(strand)
            }
            ErrorKind::Field => {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                match name.as_sym(strand) {
                    Some(sym) => Error::field(strand, sym),
                    None => {
                        let name = expect_string_like(strand, &name)?;
                        Error::field_name(strand, name)
                    }
                }
            }
            ErrorKind::UnexpectedPos => {
                let ([index], []) = unpack!(strand, args, 1, 0)?;
                let index = expect_index(strand, &index)?;
                Error::unexpected_positional(strand, index)
            }
            ErrorKind::UnexpectedKey => {
                let ([key], []) = unpack!(strand, args, 1, 0)?;
                Error::unexpected_key(strand, key)
            }
            ErrorKind::MissingPos => {
                let ([index], []) = unpack!(strand, args, 1, 0)?;
                let index = expect_index(strand, &index)?;
                Error::missing_positional(strand, index)
            }
            ErrorKind::MissingKey => {
                let ([key], []) = unpack!(strand, args, 1, 0)?;
                Error::missing_key(strand, key)
            }
            ErrorKind::Overflow => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::overflow(strand)
            }
            ErrorKind::ZeroDiv => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::zero_div(strand)
            }
            ErrorKind::SinkStop => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::sink_stop(strand)
            }
            ErrorKind::IterStop => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::iter_stop(strand)
            }
            ErrorKind::CyclicImport => {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_string_like(strand, &name)?;
                Error::cyclic_import(strand, name.as_ref())
            }
            ErrorKind::Import => {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_string_like(strand, &name)?;
                Error::import(strand, name.as_ref())
            }
            ErrorKind::Compile => {
                let ([msg], []) = unpack!(strand, args, 1, 0)?;
                let msg = expect_string_like(strand, &msg)?;
                Error::compile(strand, msg)
            }
            ErrorKind::Bytecode | ErrorKind::Interrupt => {
                return Err(Error::type_error(
                    strand,
                    format!("{} is not instantiable", variant_name(kind)),
                ));
            }
            ErrorKind::Runtime => {
                let ([msg], []) = unpack!(strand, args, 1, 0)?;
                let msg = expect_string_like(strand, &msg)?;
                Error::runtime(strand, msg)
            }
            ErrorKind::Canceled => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Error::canceled(strand)
            }
        };
        error.get_value(strand, out);
        Ok(())
    }
}

fn variant_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Unsupported => "Unsupported",
        ErrorKind::Immutable => "Immutable",
        ErrorKind::Concurrency => "Concurrency",
        ErrorKind::Type => "Type",
        ErrorKind::Value => "Value",
        ErrorKind::State => "State",
        ErrorKind::Index => "Index",
        ErrorKind::Field => "Field",
        ErrorKind::UnexpectedPos => "UnexpectedPos",
        ErrorKind::UnexpectedKey => "UnexpectedKey",
        ErrorKind::MissingPos => "MissingPos",
        ErrorKind::MissingKey => "MissingKey",
        ErrorKind::Overflow => "Overflow",
        ErrorKind::ZeroDiv => "ZeroDiv",
        ErrorKind::SinkStop => "SinkStop",
        ErrorKind::IterStop => "IterStop",
        ErrorKind::CyclicImport => "CyclicImport",
        ErrorKind::Import => "Import",
        ErrorKind::Compile => "Compile",
        ErrorKind::Bytecode => "Bytecode",
        ErrorKind::Runtime => "Runtime",
        ErrorKind::Interrupt => "Interrupt",
        ErrorKind::Canceled => "Canceled",
    }
}

fn is_runtime_superkind(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unsupported
            | ErrorKind::Immutable
            | ErrorKind::Concurrency
            | ErrorKind::Type
            | ErrorKind::Value
            | ErrorKind::State
            | ErrorKind::Index
            | ErrorKind::Field
            | ErrorKind::UnexpectedPos
            | ErrorKind::UnexpectedKey
            | ErrorKind::MissingPos
            | ErrorKind::MissingKey
            | ErrorKind::Overflow
            | ErrorKind::ZeroDiv
            | ErrorKind::CyclicImport
            | ErrorKind::Import
            | ErrorKind::Compile
            | ErrorKind::Bytecode
            | ErrorKind::Runtime
    )
}

fn expect_index<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, usize> {
    let index = value
        .as_i64(strand)
        .ok_or_else(|| Error::type_error(strand, "expected int"))?;
    usize::try_from(index).map_err(|_| Error::type_error(strand, "expected non-negative int"))
}

fn expect_string_like<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, String> {
    if let Some(sym) = value.as_sym(strand) {
        return Ok(sym.as_str(strand).to_owned());
    }
    if let Some(str) = value.as_str(strand) {
        return Ok(str.to_owned());
    }
    Err(Error::type_error(strand, "expected string"))
}
